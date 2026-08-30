//! Tests for the tabline solve and paint: which blocks anchor the two edges,
//! which tabs fit the middle window, where the scroll arrows land, and the exact
//! cells and styles `draw_tabline` writes — including tab widths measured by
//! display width for wide (CJK), emoji, and combining-mark titles.

use super::*;

use crate::snapshot::{Reconnecting, ViewerChrome};

use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_layout::mode::LayoutMode;

use crate::snapshot::{ClientSnapshot, PluginUiSnapshot, SessionSnapshot, TabMeta, TabSnapshot};

/// Build a tabline-only snapshot. `tabs` are `(name, active)`. It carries no
/// panes: the tabline reads only the session name, the tab metadata, and the
/// client's lock/select/offset state.
fn snap(
    session: &str,
    tabs: &[(&str, bool)],
    tabline_offset: Option<usize>,
    lock_mode: LockMode,
    mouse_select: bool,
) -> Frame {
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
    let snapshot = RenderSnapshot {
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
                gap: 0,
            },
            tabs_metadata,
        },
        panes: Vec::new(),
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport,
            active_tab: tab_id,
            focused_pane: None,
            lock_mode,
            mouse_select,
        },
        plugin_ui: PluginUiSnapshot::default(),
    };
    Frame {
        snapshot,
        chrome: ViewerChrome {
            hovered_pane: None,
            tabline_offset,
            reconnecting: None,
        },
    }
}

/// One fixture frame: what the session handed out, plus the tab-strip position
/// the viewer paints it with.
struct Frame {
    snapshot: RenderSnapshot,
    chrome: ViewerChrome,
}

/// Cells the `[v…] ` version badge takes beside the session name: the version
/// string plus `[`, `v`, `]`, and the trailing space. Read from
/// [`KOSHI_VERSION`], so a longer version string widens this constant and every
/// expected column below with it.
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

/// Everything `draw_tabline` paints `frame` from, in `theme`'s colors.
fn navigator<'a>(frame: &'a Frame, theme: &'a Theme) -> NavigatorDto<'a> {
    NavigatorDto {
        session_name: &frame.snapshot.session.name,
        tabs: &frame.snapshot.session.tabs_metadata,
        lock_mode: frame.snapshot.client.lock_mode,
        mouse_select: frame.snapshot.client.mouse_select,
        reconnecting: frame.chrome.reconnecting,
        tabline_offset: frame.chrome.tabline_offset,
        theme,
    }
}

/// Paint the tabline into a fresh one-row buffer of `width` cells.
fn draw(frame: &Frame, width: u16) -> Buffer {
    let a = area(width);
    let mut buf = Buffer::empty(a);
    let theme = Theme::default();
    draw_tabline(&navigator(frame, &theme), a, &mut buf);
    buf
}

/// Solve the tabline for `frame` over `area`.
fn solve_tabline(frame: &Frame, area: RatatuiRect) -> TablineLayout {
    tabline_layout(frame.snapshot.layout(frame.chrome).navigator(), area)
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
    let layout = solve_tabline(
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
    let layout = solve_tabline(
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
    // One cell wider than the tab needs: the tab's cells end exactly where the
    // right block starts, so it just fits and no scrolling begins.
    let layout = solve_tabline(
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
    let layout = solve_tabline(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        area(16 + BADGE),
    );
    assert_eq!(layout.first_visible, 0);
    assert!(layout.tabs.is_empty());
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, Some((9 + BADGE, 1)));
}

#[test]
fn two_tabs_fit_when_the_row_holds_both_plus_the_gap_between_them() {
    // Two 7-cell tabs with the one-cell gap need 15 cells of strip. At exactly
    // that they both show unscrolled; one cell less drops the second tab and
    // starts the scrolling window.
    let frame = snap(
        "s",
        &[("a", true), ("b", false)],
        None,
        LockMode::Normal,
        false,
    );

    let fits = solve_tabline(&frame, area(25 + BADGE));
    assert_eq!(fits.first_visible, 0);
    assert_eq!(fits.tabs, vec![(0, 4 + BADGE, 7), (1, 12 + BADGE, 7)]);
    assert_eq!(fits.left_arrow, None);
    assert_eq!(fits.right_arrow, None);

    let one_short = solve_tabline(&frame, area(24 + BADGE));
    assert_eq!(one_short.first_visible, 0);
    assert_eq!(one_short.tabs, vec![(0, 5 + BADGE, 7)]);
    assert_eq!(one_short.left_arrow, None);
    assert_eq!(one_short.right_arrow, Some((17 + BADGE, 1)));
}

#[test]
fn following_the_active_tab_scrolls_it_into_view() {
    // The strip holds one tab in the arrow-framed window; with the last tab
    // active and no peek offset, the window starts at it and only a left arrow
    // shows.
    let layout = solve_tabline(
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
fn the_window_starts_at_the_smallest_index_that_still_shows_the_active_tab() {
    // The arrow-framed window is 16 cells, room for two 7-cell tabs and the gap
    // between them. With the last of four tabs active, the window starts at tab
    // 2 — not at tab 3 — so the active tab sits at the right edge with its
    // neighbour beside it.
    let layout = solve_tabline(
        &snap(
            "s",
            &[("a", false), ("b", false), ("c", false), ("d", true)],
            None,
            LockMode::Normal,
            false,
        ),
        area(28 + BADGE),
    );
    assert_eq!(layout.first_visible, 2);
    assert_eq!(layout.tabs, vec![(2, 5 + BADGE, 7), (3, 13 + BADGE, 7)]);
    assert_eq!(layout.left_arrow, Some((4 + BADGE, 1)));
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn a_peek_offset_windows_from_that_index_with_both_arrows() {
    let layout = solve_tabline(
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
    let layout = solve_tabline(
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
fn a_peek_offset_of_zero_holds_the_window_at_the_first_tab() {
    // The same fixture the active-tab case scrolls to index 2: an offset of 0
    // pins the window at the first tab instead, with only a right arrow.
    let layout = solve_tabline(
        &snap(
            "s",
            &[("a", false), ("b", false), ("c", true)],
            Some(0),
            LockMode::Normal,
            false,
        ),
        area(24 + BADGE),
    );
    assert_eq!(layout.first_visible, 0);
    assert_eq!(layout.tabs, vec![(0, 5 + BADGE, 7)]);
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, Some((17 + BADGE, 1)));
}

#[test]
fn a_peek_offset_is_ignored_while_every_tab_fits() {
    // The strip only scrolls once a tab is hidden. On a row wide enough for all
    // three, a peek at index 2 still shows them all from index 0.
    let layout = solve_tabline(
        &snap(
            "s",
            &[("a", true), ("b", false), ("c", false)],
            Some(2),
            LockMode::Normal,
            false,
        ),
        area(40 + BADGE),
    );
    assert_eq!(layout.first_visible, 0);
    assert_eq!(
        layout.tabs,
        vec![(0, 4 + BADGE, 7), (1, 12 + BADGE, 7), (2, 20 + BADGE, 7)]
    );
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn no_active_tab_windows_from_the_first() {
    let layout = solve_tabline(
        &snap(
            "s",
            &[("a", false), ("b", false), ("c", false)],
            None,
            LockMode::Normal,
            false,
        ),
        area(24 + BADGE),
    );
    assert_eq!(layout.first_visible, 0);
    assert_eq!(layout.tabs, vec![(0, 5 + BADGE, 7)]);
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, Some((17 + BADGE, 1)));
}

#[test]
fn the_first_active_tab_wins_when_several_are_marked() {
    // Tabs 0 and 2 both claim active; the window follows tab 0.
    let layout = solve_tabline(
        &snap(
            "s",
            &[("a", true), ("b", false), ("c", true)],
            None,
            LockMode::Normal,
            false,
        ),
        area(24 + BADGE),
    );
    assert_eq!(layout.first_visible, 0);
    assert_eq!(layout.tabs, vec![(0, 5 + BADGE, 7)]);
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, Some((17 + BADGE, 1)));
}

#[test]
fn an_active_tab_wider_than_the_window_shows_no_tabs_between_both_arrows() {
    // The `verylongname` tab at index 1 is 18 cells wide and the arrow-framed
    // window is 12: the window starts at it, holds nothing, and both arrows
    // mark the hidden sides.
    let layout = solve_tabline(
        &snap(
            "s",
            &[("a", false), ("verylongname", true), ("c", false)],
            None,
            LockMode::Normal,
            false,
        ),
        area(24 + BADGE),
    );
    assert_eq!(layout.first_visible, 1);
    assert!(layout.tabs.is_empty());
    assert_eq!(layout.left_arrow, Some((4 + BADGE, 0)));
    assert_eq!(layout.right_arrow, Some((17 + BADGE, 2)));
}

#[test]
fn an_empty_tab_list_leaves_only_the_two_blocks() {
    let layout = solve_tabline(
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
fn a_zero_width_row_yields_no_blocks_and_no_tabs() {
    let layout = solve_tabline(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        area(0),
    );
    assert_eq!(layout.session_width, 0);
    assert_eq!(layout.right_x, 0);
    assert_eq!(layout.first_visible, 0);
    assert!(layout.tabs.is_empty());
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn a_row_offset_from_the_origin_places_every_block_from_its_x() {
    // The same 20 + BADGE row starting at column 10: every column below is the
    // origin case shifted by 10, and the right block still ends at the row's
    // right edge.
    let offset = RatatuiRect {
        x: 10,
        y: 0,
        width: 20 + BADGE,
        height: 1,
    };
    let layout = solve_tabline(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        offset,
    );
    assert_eq!(layout.session_width, 3 + BADGE);
    assert_eq!(layout.right_x, 24 + BADGE);
    assert_eq!(layout.tabs, vec![(0, 14 + BADGE, 7)]);
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn an_offset_row_narrower_than_the_mode_tag_anchors_the_right_block_at_its_x() {
    // A 3-cell row starting at column 10 has no room for the 6-cell " BASE "
    // block: the block starts at the row's own x, never left of it.
    let layout = solve_tabline(
        &snap("s", &[("a", true)], None, LockMode::Normal, false),
        RatatuiRect {
            x: 10,
            y: 0,
            width: 3,
            height: 1,
        },
    );
    assert_eq!(layout.right_x, 10);
    assert_eq!(layout.session_width, 0);
    assert!(layout.tabs.is_empty());
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn no_room_between_the_blocks_yields_no_tabs() {
    // width 6 is exactly the right block, leaving no strip at all.
    let layout = solve_tabline(
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
    let layout = solve_tabline(
        &snap("s", &[("a", true)], None, LockMode::Normal, true),
        area(20 + BADGE),
    );
    assert_eq!(layout.right_x, 12 + BADGE);
}

#[test]
fn a_lock_mode_tag_is_the_same_width_as_base() {
    // " LOCK " and " BASE " are both 6 cells.
    let layout = solve_tabline(
        &snap("s", &[("a", true)], None, LockMode::Locked, false),
        area(20 + BADGE),
    );
    assert_eq!(layout.right_x, 14 + BADGE);
}

#[test]
fn a_composed_mode_tag_pushes_the_right_block_further_left() {
    // Locked and selecting at once joins both tags: " LOCK · SELECT " is 15
    // cells, so the right block starts 15 cells from the row's right edge.
    let frame = snap("s", &[("a", true)], None, LockMode::Locked, true);
    let block = " LOCK · SELECT ";
    assert_eq!(
        right_block_text(frame.snapshot.layout(frame.chrome).navigator()),
        block
    );
    assert_eq!(text_width(block), 15);

    let width = 30 + BADGE;
    assert_eq!(solve_tabline(&frame, area(width)).right_x, width - 15);

    let buf = draw(&frame, width);
    let text: String = (width - 15..width).map(|x| cell(&buf, x)).collect();
    assert_eq!(text, block);
}

// --- display-width titles ----------------------------------------------------

#[test]
fn a_wide_cjk_title_counts_two_cells_per_glyph() {
    // " 字 " is 1 + 2 + 1 = 4 cells, so the tab is " #1 "(4) + 4 = 8 wide.
    let layout = solve_tabline(
        &snap("s", &[("字", true)], None, LockMode::Normal, false),
        area(60 + BADGE),
    );
    assert_eq!(layout.tabs, vec![(0, 4 + BADGE, 8)]);
}

#[test]
fn an_emoji_title_counts_two_cells() {
    let layout = solve_tabline(
        &snap("s", &[("🎉", true)], None, LockMode::Normal, false),
        area(60 + BADGE),
    );
    assert_eq!(layout.tabs, vec![(0, 4 + BADGE, 8)]);
}

#[test]
fn a_combining_mark_title_stays_one_cell() {
    // "e" + combining acute is one display cell: " é " is 3, tab is 4 + 3 = 7.
    let layout = solve_tabline(
        &snap("s", &[("e\u{0301}", true)], None, LockMode::Normal, false),
        area(60 + BADGE),
    );
    assert_eq!(layout.tabs, vec![(0, 4 + BADGE, 7)]);
}

#[test]
fn a_wide_session_name_widens_the_left_block() {
    // " 字 " is 1 + 2 + 1 = 4 cells, one more than " s ", so the strip and its
    // first tab start one cell further right.
    let layout = solve_tabline(
        &snap("字", &[("a", true)], None, LockMode::Normal, false),
        area(20 + BADGE),
    );
    assert_eq!(layout.session_width, 4 + BADGE);
    assert_eq!(layout.tabs, vec![(0, 5 + BADGE, 7)]);
}

#[test]
fn empty_session_and_tab_names_keep_their_padding() {
    // An empty session name is still the 2-cell block "  ", and a tab with an
    // empty name is " #1 "(4) plus "  "(2) — a 6-cell ribbon.
    let frame = snap("", &[("", true)], None, LockMode::Normal, false);
    let layout = solve_tabline(&frame, area(20 + BADGE));
    assert_eq!(layout.session_width, 2 + BADGE);
    assert_eq!(layout.tabs, vec![(0, 3 + BADGE, 6)]);

    let buf = draw(&frame, 20 + BADGE);
    let tab = 3 + BADGE;
    let text: String = (tab..tab + 6).map(|x| cell(&buf, x)).collect();
    assert_eq!(text, " #1   ");
}

#[test]
fn a_two_digit_tab_number_widens_that_tab() {
    // Tab 9 shows "#10" — a wider `#N` block than the single-digit tabs.
    let tabs: Vec<(&str, bool)> = (0..10).map(|i| ("a", i == 0)).collect();
    let layout = solve_tabline(
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

    // Then the version badge `[v…] `: the same ramp color as the name, without
    // its bold. The expected text is spelled out here rather than taken from
    // `version_badge`, pinning the badge's shape.
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
fn an_inactive_tab_paints_the_dimmed_ramp_as_its_block_background() {
    // Two tabs, the second active. The inactive one is quiet text on the dimmed
    // ramp stop, in no bold; the active one is its ramp stop as text and keeps
    // the bar background, bold on the `#N` block and plain on the name block.
    let buf = draw(
        &snap(
            "s",
            &[("a", false), ("b", true)],
            None,
            LockMode::Normal,
            false,
        ),
        40 + BADGE,
    );

    let inactive = 4 + BADGE;
    assert_eq!(cell(&buf, inactive + 1), "#");
    assert_eq!(buf[(inactive + 1, 0)].fg, Color::Rgb(0xf0, 0xec, 0xfa));
    assert_eq!(buf[(inactive + 1, 0)].bg, Color::Rgb(0x72, 0x5a, 0x8c));
    assert!(!buf[(inactive + 1, 0)].modifier.contains(Modifier::BOLD));
    assert_eq!(cell(&buf, inactive + 5), "a");
    assert_eq!(buf[(inactive + 5, 0)].bg, Color::Rgb(0x72, 0x5a, 0x8c));

    let active = 12 + BADGE;
    assert_eq!(cell(&buf, active + 1), "#");
    assert_eq!(buf[(active + 1, 0)].fg, Color::Rgb(0x7d, 0xbc, 0xff));
    assert_eq!(buf[(active + 1, 0)].bg, Color::Rgb(0x00, 0x00, 0x00));
    assert!(buf[(active + 1, 0)].modifier.contains(Modifier::BOLD));
    assert_eq!(cell(&buf, active + 5), "b");
    assert_eq!(buf[(active + 5, 0)].fg, Color::Rgb(0x7d, 0xbc, 0xff));
    assert!(!buf[(active + 5, 0)].modifier.contains(Modifier::BOLD));
}

#[test]
fn a_row_below_the_buffer_leaves_every_cell_untouched() {
    // A resize can leave the committed tabline row past the buffer's last row.
    // Nothing is painted there and no cell of the buffer changes.
    let frame = snap("s", &[("a", true)], None, LockMode::Normal, false);
    let theme = Theme::default();
    let mut buf = Buffer::empty(area(20 + BADGE));
    buf[(0, 0)].set_symbol("x");
    let before = buf.clone();

    let below = RatatuiRect {
        x: 0,
        y: 1,
        width: 20 + BADGE,
        height: 1,
    };
    draw_tabline(&navigator(&frame, &theme), below, &mut buf);
    assert_eq!(buf, before);
}

#[test]
fn a_row_too_narrow_for_the_badge_drops_it_whole() {
    // 16 cells hold the session block and the " BASE " tag but not the badge
    // as well. The badge is dropped entire rather than cut off part-way. The
    // tab does not fit either, so the strip is just its right arrow.
    let snapshot = snap("s", &[("a", true)], None, LockMode::Normal, false);
    let buf = draw(&snapshot, 16);
    let row: String = (0..16).map(|x| cell(&buf, x)).collect();
    assert_eq!(row, " s       ▶ BASE ");
    assert_eq!(solve_tabline(&snapshot, area(16)).session_width, 3);
}

#[test]
fn a_row_narrower_than_the_mode_tag_clips_the_tag_at_the_right_edge() {
    // 3 cells hold neither block whole: the right block starts at column 0 and
    // keeps its first 3 cells, and the session block gets no room at all.
    let frame = snap("s", &[("a", true)], None, LockMode::Normal, false);
    let layout = solve_tabline(&frame, area(3));
    assert_eq!(layout.session_width, 0);
    assert_eq!(layout.right_x, 0);
    assert!(layout.tabs.is_empty());

    let buf = draw(&frame, 3);
    let row: String = (0..3).map(|x| cell(&buf, x)).collect();
    assert_eq!(row, " BA");
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
fn draw_paints_the_reconnecting_tag_while_the_viewer_has_no_link() {
    let mut frame = snap("s", &[("a", true)], None, LockMode::Normal, false);
    frame.chrome.reconnecting = Some(Reconnecting {
        attempt: 3,
        retry_in_seconds: 8,
    });
    // " RECONNECTING (attempt 3, retry in 8s) " fills the row's last 39 cells,
    // and the row is 16 + BADGE cells wider than that block.
    let block = " RECONNECTING (attempt 3, retry in 8s) ";
    assert_eq!(
        right_block_text(frame.snapshot.layout(frame.chrome).navigator()),
        block
    );
    let tag_width = text_width(block);
    assert_eq!(tag_width, 39);
    let width = tag_width + 16 + BADGE;
    let buf = draw(&frame, width);
    let text: String = (width - tag_width..width).map(|x| cell(&buf, x)).collect();
    assert_eq!(text, block);
}

#[test]
fn draw_paints_the_base_tag_while_the_viewer_has_a_link() {
    let width = 30 + BADGE;
    let frame = snap("s", &[("a", true)], None, LockMode::Normal, false);
    let buf = draw(&frame, width);
    // The same row with the link up ends in the 6-cell " BASE " block.
    let tag = width - 6;
    let text: String = (tag..width).map(|x| cell(&buf, x)).collect();
    assert_eq!(text, " BASE ");
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

#[test]
fn an_absurdly_long_name_saturates_instead_of_wrapping() {
    // Session and tab names are unbounded strings — a profile file can set one
    // of any length. A name past `u16::MAX` cells measures as `u16::MAX`, which
    // reads as wider than the row rather than wrapping to a small number.
    let huge = "x".repeat(usize::from(u16::MAX) + 64);
    assert_eq!(text_width(&huge), u16::MAX);

    // The solve still answers, and the oversized name claims exactly the 34
    // cells left of the 6-cell " BASE " block — never more than the row.
    let frame = snap(&huge, &[("one", true)], None, LockMode::Normal, false);
    let layout = solve_tabline(
        &frame,
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 1,
        },
    );
    assert_eq!(layout.session_width, 34);
    assert_eq!(layout.right_x, 34);
    assert_eq!(layout.first_visible, 0);
    assert!(layout.tabs.is_empty());
    assert_eq!(layout.left_arrow, None);
    assert_eq!(layout.right_arrow, None);
}

#[test]
fn text_width_counts_display_cells_not_bytes_or_chars() {
    // The solve places blocks in terminal cells, so measuring must use display
    // width. "漢字" is 2 chars and 6 bytes but occupies 4 cells; an emoji is
    // 1 char and 4 bytes but occupies 2; a combining mark adds none.
    assert_eq!(text_width("漢字"), 4);
    assert_eq!(text_width("🦀"), 2);
    assert_eq!(
        text_width("e\u{0301}"),
        1,
        "e + combining acute is one cell"
    );
    assert_eq!(text_width(""), 0);
}

#[test]
fn the_version_badge_is_kept_at_exactly_enough_room_and_dropped_one_cell_short() {
    // The badge is all-or-nothing: it fits or it goes whole, never clipped.
    // This pins the `<=` boundary the session block's width is solved from.
    let fixture = snap("s", &[("one", true)], None, LockMode::Normal, false);
    let layout = fixture.snapshot.layout(fixture.chrome);
    let frame = layout.navigator();

    let full = session_texts(frame.session_name, u16::MAX);
    let name_width = text_width(&full.name);
    let exactly_enough = name_width + text_width(&version_badge());

    let kept = session_texts(frame.session_name, exactly_enough);
    assert_eq!(kept.name, " s ");
    assert_eq!(kept.badge, Some(version_badge()));
    assert_eq!(kept.width, exactly_enough);

    let dropped = session_texts(frame.session_name, exactly_enough - 1);
    assert_eq!(dropped.name, " s ");
    assert_eq!(dropped.badge, None);
    assert_eq!(dropped.width, name_width);
}

#[test]
fn the_session_block_reports_its_own_width_even_with_no_room_for_it() {
    // With `room` at 0 the badge goes and the name stays: the block reports the
    // 3 cells " s " needs, and `tabline_layout` is what clamps that to the row.
    let block = session_texts("s", 0);
    assert_eq!(block.name, " s ");
    assert_eq!(block.badge, None);
    assert_eq!(block.width, 3);
}
