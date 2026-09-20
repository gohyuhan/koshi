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

/// Build a tabline-only snapshot. `tab_names_and_activity` are `(name, active)`.
/// It carries no
/// panes: the tabline reads only the session name, the tab metadata, and the
/// client's lock/select/offset state.
fn build_tabline_frame(
    session_name: &str,
    tab_names_and_activity: &[(&str, bool)],
    tabline_offset: Option<usize>,
    lock_mode: LockMode,
    is_mouse_selection_enabled: bool,
) -> TablineTestFrame {
    let tab_id = TabId::new();
    let tabs_metadata = tab_names_and_activity
        .iter()
        .enumerate()
        .map(|(tab_index, (tab_name, is_active))| TabMeta {
            tab_id: TabId::new(),
            tab_name: (*tab_name).to_string(),
            tab_index,
            is_active: *is_active,
        })
        .collect();
    let viewport_size = Size {
        column_count: 200,
        row_count: 1,
    };
    let render_snapshot = RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_revision: 0,
            session_name: session_name.to_string(),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: "active".to_string(),
                pane_slots: Vec::new(),
                effective_cell_size: viewport_size,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata,
        },
        pane_snapshots: Vec::new(),
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            client_revision: 0,
            viewport_size,
            active_tab_id: tab_id,
            focused_pane_id: None,
            lock_mode,
            is_mouse_selection_enabled,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    };
    TablineTestFrame {
        render_snapshot,
        viewer_chrome: ViewerChrome {
            hovered_pane_id: None,
            tabline_offset,
            reconnecting: None,
        },
    }
}

/// One fixture frame: what the session handed out, plus the tab-strip position
/// the viewer paints it with.
struct TablineTestFrame {
    render_snapshot: RenderSnapshot,
    viewer_chrome: ViewerChrome,
}

fn build_visible_tab_span(
    tab_metadata_index: usize,
    start_column: u16,
    column_count: u16,
) -> VisibleTabSpan {
    VisibleTabSpan {
        tab_metadata_index,
        start_column,
        column_count,
    }
}

fn build_tabline_scroll_arrow(
    start_column: u16,
    target_first_visible_tab_index: usize,
) -> TablineScrollArrow {
    TablineScrollArrow {
        start_column,
        target_first_visible_tab_index,
    }
}

/// Cells the `[v…] ` version badge takes beside the session name: the version
/// string plus `[`, `v`, `]`, and the trailing space. Read from
/// [`KOSHI_VERSION`], so a longer version string widens this constant and every
/// expected column below with it.
const VERSION_BADGE_COLUMN_COUNT: u16 = KOSHI_VERSION.len() as u16 + 4;

/// A one-row render area `column_count` cells wide, anchored at the origin.
fn build_tabline_area(column_count: u16) -> RatatuiRect {
    RatatuiRect {
        x: 0,
        y: 0,
        width: column_count,
        height: 1,
    }
}

/// Everything `draw_tabline` paints from `tabline_test_frame`.
fn build_tabline_inputs(tabline_test_frame: &TablineTestFrame) -> TablineInputs<'_> {
    TablineInputs {
        session_name: &tabline_test_frame
            .render_snapshot
            .session_snapshot
            .session_name,
        tabs_metadata: &tabline_test_frame
            .render_snapshot
            .session_snapshot
            .tabs_metadata,
        lock_mode: tabline_test_frame.render_snapshot.client_snapshot.lock_mode,
        is_mouse_selection_enabled: tabline_test_frame
            .render_snapshot
            .client_snapshot
            .is_mouse_selection_enabled,
        reconnecting: tabline_test_frame.viewer_chrome.reconnecting,
        tabline_offset: tabline_test_frame.viewer_chrome.tabline_offset,
    }
}

/// Paint the tabline into a fresh one-row buffer of `column_count` cells.
fn render_tabline(tabline_test_frame: &TablineTestFrame, column_count: u16) -> Buffer {
    let tabline_area = build_tabline_area(column_count);
    let mut render_buffer = Buffer::empty(tabline_area);
    let render_theme = Theme::default();
    draw_tabline(
        build_tabline_inputs(tabline_test_frame),
        &render_theme,
        tabline_area,
        &mut render_buffer,
    );
    render_buffer
}

/// Solve the tabline for `tabline_test_frame` over `tabline_area`.
fn solve_tabline_layout(
    tabline_test_frame: &TablineTestFrame,
    tabline_area: RatatuiRect,
) -> TablineLayout {
    super::solve_tabline_layout(
        tabline_test_frame
            .render_snapshot
            .build_frame_layout(tabline_test_frame.viewer_chrome)
            .get_tabline_inputs(),
        tabline_area,
    )
}

/// The symbol at `column_index` of the single rendered row.
fn get_rendered_cell_symbol(render_buffer: &Buffer, column_index: u16) -> &str {
    render_buffer[(column_index, 0)].symbol()
}

// --- geometry: block widths and the fitting window ---------------------------

#[test]
fn one_tab_that_fits_shows_whole_with_no_arrows() {
    // Session block ` s ` (3 cells) plus the version badge; right block
    // " BASE " = 6; the strip starts one cell past the session block, so the
    // tab " #1  a " (7 cells) sits just after the badge.
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false),
        build_tabline_area(20 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.session_block_width, 3 + VERSION_BADGE_COLUMN_COUNT);
    assert_eq!(
        layout.right_block_start_column,
        14 + VERSION_BADGE_COLUMN_COUNT
    );
    assert_eq!(layout.first_visible_tab_index, 0);
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 4 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn several_tabs_that_all_fit_pack_left_to_right_with_a_gap() {
    let layout = solve_tabline_layout(
        &build_tabline_frame(
            "s",
            &[("a", false), ("b", false), ("c", true)],
            None,
            LockMode::Normal,
            false,
        ),
        build_tabline_area(40 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(
        layout.visible_tab_spans,
        vec![
            build_visible_tab_span(0, 4 + VERSION_BADGE_COLUMN_COUNT, 7),
            build_visible_tab_span(1, 12 + VERSION_BADGE_COLUMN_COUNT, 7),
            build_visible_tab_span(2, 20 + VERSION_BADGE_COLUMN_COUNT, 7),
        ]
    );
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn a_tab_that_exactly_fills_the_gap_is_kept() {
    // One cell wider than the tab needs: the tab's cells end exactly where the
    // right block starts, so it just fits and no scrolling begins.
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false),
        build_tabline_area(17 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 4 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn one_column_too_narrow_drops_the_tab_and_shows_a_right_arrow() {
    // One cell narrower than that: the tab no longer fits, so the strip
    // scrolls; nothing is visible yet and a right arrow marks the tab hidden
    // off the right edge.
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false),
        build_tabline_area(16 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.first_visible_tab_index, 0);
    assert!(layout.visible_tab_spans.is_empty());
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(
        layout.right_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            9 + VERSION_BADGE_COLUMN_COUNT,
            1
        ))
    );
}

#[test]
fn two_tabs_fit_when_the_row_holds_both_plus_the_gap_between_them() {
    // Two 7-cell tabs with the one-cell gap need 15 cells of strip. At exactly
    // that they both show unscrolled; one cell less drops the second tab and
    // starts the scrolling window.
    let tabline_test_frame = build_tabline_frame(
        "s",
        &[("a", true), ("b", false)],
        None,
        LockMode::Normal,
        false,
    );

    let fitting_layout = solve_tabline_layout(
        &tabline_test_frame,
        build_tabline_area(25 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(fitting_layout.first_visible_tab_index, 0);
    assert_eq!(
        fitting_layout.visible_tab_spans,
        vec![
            build_visible_tab_span(0, 4 + VERSION_BADGE_COLUMN_COUNT, 7),
            build_visible_tab_span(1, 12 + VERSION_BADGE_COLUMN_COUNT, 7),
        ]
    );
    assert_eq!(fitting_layout.left_scroll_arrow, None);
    assert_eq!(fitting_layout.right_scroll_arrow, None);

    let one_cell_short_layout = solve_tabline_layout(
        &tabline_test_frame,
        build_tabline_area(24 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(one_cell_short_layout.first_visible_tab_index, 0);
    assert_eq!(
        one_cell_short_layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 5 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
    assert_eq!(one_cell_short_layout.left_scroll_arrow, None);
    assert_eq!(
        one_cell_short_layout.right_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            17 + VERSION_BADGE_COLUMN_COUNT,
            1
        ))
    );
}

#[test]
fn following_the_active_tab_scrolls_it_into_view() {
    // The strip holds one tab in the arrow-framed window; with the last tab
    // active and no peek offset, the window starts at it and only a left arrow
    // shows.
    let layout = solve_tabline_layout(
        &build_tabline_frame(
            "s",
            &[("a", false), ("b", false), ("c", true)],
            None,
            LockMode::Normal,
            false,
        ),
        build_tabline_area(24 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.first_visible_tab_index, 2);
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(2, 5 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
    assert_eq!(
        layout.left_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            4 + VERSION_BADGE_COLUMN_COUNT,
            1
        ))
    );
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn the_window_starts_at_the_smallest_index_that_still_shows_the_active_tab() {
    // The arrow-framed window is 16 cells, room for two 7-cell tabs and the gap
    // between them. With the last of four tabs active, the window starts at tab
    // 2 — not at tab 3 — so the active tab sits at the right edge with its
    // neighbour beside it.
    let layout = solve_tabline_layout(
        &build_tabline_frame(
            "s",
            &[("a", false), ("b", false), ("c", false), ("d", true)],
            None,
            LockMode::Normal,
            false,
        ),
        build_tabline_area(28 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.first_visible_tab_index, 2);
    assert_eq!(
        layout.visible_tab_spans,
        vec![
            build_visible_tab_span(2, 5 + VERSION_BADGE_COLUMN_COUNT, 7),
            build_visible_tab_span(3, 13 + VERSION_BADGE_COLUMN_COUNT, 7),
        ]
    );
    assert_eq!(
        layout.left_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            4 + VERSION_BADGE_COLUMN_COUNT,
            1
        ))
    );
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn a_peek_offset_windows_from_that_index_with_both_arrows() {
    let layout = solve_tabline_layout(
        &build_tabline_frame(
            "s",
            &[("a", true), ("b", false), ("c", false)],
            Some(1),
            LockMode::Normal,
            false,
        ),
        build_tabline_area(24 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.first_visible_tab_index, 1);
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(1, 5 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
    assert_eq!(
        layout.left_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            4 + VERSION_BADGE_COLUMN_COUNT,
            0
        ))
    );
    assert_eq!(
        layout.right_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            17 + VERSION_BADGE_COLUMN_COUNT,
            2
        ))
    );
}

#[test]
fn a_peek_offset_past_the_last_tab_clamps_to_it() {
    let layout = solve_tabline_layout(
        &build_tabline_frame(
            "s",
            &[("a", true), ("b", false), ("c", false)],
            Some(99),
            LockMode::Normal,
            false,
        ),
        build_tabline_area(24 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.first_visible_tab_index, 2);
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(2, 5 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
    assert_eq!(
        layout.left_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            4 + VERSION_BADGE_COLUMN_COUNT,
            1
        ))
    );
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn a_peek_offset_of_zero_holds_the_window_at_the_first_tab() {
    // The same fixture the active-tab case scrolls to index 2: an offset of 0
    // pins the window at the first tab instead, with only a right arrow.
    let layout = solve_tabline_layout(
        &build_tabline_frame(
            "s",
            &[("a", false), ("b", false), ("c", true)],
            Some(0),
            LockMode::Normal,
            false,
        ),
        build_tabline_area(24 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.first_visible_tab_index, 0);
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 5 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(
        layout.right_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            17 + VERSION_BADGE_COLUMN_COUNT,
            1
        ))
    );
}

#[test]
fn a_peek_offset_is_ignored_while_every_tab_fits() {
    // The strip only scrolls once a tab is hidden. On a row wide enough for all
    // three, a peek at index 2 still shows them all from index 0.
    let layout = solve_tabline_layout(
        &build_tabline_frame(
            "s",
            &[("a", true), ("b", false), ("c", false)],
            Some(2),
            LockMode::Normal,
            false,
        ),
        build_tabline_area(40 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.first_visible_tab_index, 0);
    assert_eq!(
        layout.visible_tab_spans,
        vec![
            build_visible_tab_span(0, 4 + VERSION_BADGE_COLUMN_COUNT, 7),
            build_visible_tab_span(1, 12 + VERSION_BADGE_COLUMN_COUNT, 7),
            build_visible_tab_span(2, 20 + VERSION_BADGE_COLUMN_COUNT, 7),
        ]
    );
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn no_active_tab_windows_from_the_first() {
    let layout = solve_tabline_layout(
        &build_tabline_frame(
            "s",
            &[("a", false), ("b", false), ("c", false)],
            None,
            LockMode::Normal,
            false,
        ),
        build_tabline_area(24 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.first_visible_tab_index, 0);
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 5 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(
        layout.right_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            17 + VERSION_BADGE_COLUMN_COUNT,
            1
        ))
    );
}

#[test]
fn the_first_active_tab_wins_when_several_are_marked() {
    // Tabs 0 and 2 both claim active; the window follows tab 0.
    let layout = solve_tabline_layout(
        &build_tabline_frame(
            "s",
            &[("a", true), ("b", false), ("c", true)],
            None,
            LockMode::Normal,
            false,
        ),
        build_tabline_area(24 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.first_visible_tab_index, 0);
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 5 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(
        layout.right_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            17 + VERSION_BADGE_COLUMN_COUNT,
            1
        ))
    );
}

#[test]
fn an_active_tab_wider_than_the_window_shows_no_tabs_between_both_arrows() {
    // The `verylongname` tab at index 1 is 18 cells wide and the arrow-framed
    // window is 12: the window starts at it, holds nothing, and both arrows
    // mark the hidden sides.
    let layout = solve_tabline_layout(
        &build_tabline_frame(
            "s",
            &[("a", false), ("verylongname", true), ("c", false)],
            None,
            LockMode::Normal,
            false,
        ),
        build_tabline_area(24 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.first_visible_tab_index, 1);
    assert!(layout.visible_tab_spans.is_empty());
    assert_eq!(
        layout.left_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            4 + VERSION_BADGE_COLUMN_COUNT,
            0
        ))
    );
    assert_eq!(
        layout.right_scroll_arrow,
        Some(build_tabline_scroll_arrow(
            17 + VERSION_BADGE_COLUMN_COUNT,
            2
        ))
    );
}

#[test]
fn an_empty_tab_list_leaves_only_the_two_blocks() {
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[], None, LockMode::Normal, false),
        build_tabline_area(20 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.session_block_width, 3 + VERSION_BADGE_COLUMN_COUNT);
    assert_eq!(
        layout.right_block_start_column,
        14 + VERSION_BADGE_COLUMN_COUNT
    );
    assert_eq!(layout.first_visible_tab_index, 0);
    assert!(layout.visible_tab_spans.is_empty());
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn a_zero_width_row_yields_no_blocks_and_no_tabs() {
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false),
        build_tabline_area(0),
    );
    assert_eq!(layout.session_block_width, 0);
    assert_eq!(layout.right_block_start_column, 0);
    assert_eq!(layout.first_visible_tab_index, 0);
    assert!(layout.visible_tab_spans.is_empty());
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn a_row_offset_from_the_origin_places_every_block_from_its_x() {
    // The same 20 + VERSION_BADGE_COLUMN_COUNT row starting at column 10: every column below is the
    // origin case shifted by 10, and the right block still ends at the row's
    // right edge.
    let tabline_area = RatatuiRect {
        x: 10,
        y: 0,
        width: 20 + VERSION_BADGE_COLUMN_COUNT,
        height: 1,
    };
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false),
        tabline_area,
    );
    assert_eq!(layout.session_block_width, 3 + VERSION_BADGE_COLUMN_COUNT);
    assert_eq!(
        layout.right_block_start_column,
        24 + VERSION_BADGE_COLUMN_COUNT
    );
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(
            0,
            14 + VERSION_BADGE_COLUMN_COUNT,
            7
        )]
    );
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn an_offset_row_narrower_than_the_mode_tag_anchors_the_right_block_at_its_x() {
    // A 3-cell row starting at column 10 has no room for the 6-cell " BASE "
    // block: the block starts at the row's own x, never left of it.
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false),
        RatatuiRect {
            x: 10,
            y: 0,
            width: 3,
            height: 1,
        },
    );
    assert_eq!(layout.right_block_start_column, 10);
    assert_eq!(layout.session_block_width, 0);
    assert!(layout.visible_tab_spans.is_empty());
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn no_room_between_the_blocks_yields_no_tabs() {
    // width 6 is exactly the right block, leaving no strip at all.
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false),
        build_tabline_area(6),
    );
    assert_eq!(layout.session_block_width, 0);
    assert_eq!(layout.right_block_start_column, 0);
    assert!(layout.visible_tab_spans.is_empty());
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(layout.right_scroll_arrow, None);
}

// --- the mode tag drives the right block's width -----------------------------

#[test]
fn the_select_mode_tag_widens_the_right_block() {
    // " SELECT " is 8 cells, so the right block starts 8 cells from the right
    // edge — two cells left of where the 6-cell " BASE " block starts.
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, true),
        build_tabline_area(20 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(
        layout.right_block_start_column,
        12 + VERSION_BADGE_COLUMN_COUNT
    );
}

#[test]
fn a_lock_mode_tag_is_the_same_width_as_base() {
    // " LOCK " and " BASE " are both 6 cells.
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Locked, false),
        build_tabline_area(20 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(
        layout.right_block_start_column,
        14 + VERSION_BADGE_COLUMN_COUNT
    );
}

#[test]
fn a_composed_mode_tag_pushes_the_right_block_further_left() {
    // Locked and selecting at once joins both tags: " LOCK · SELECT " is 15
    // cells, so the right block starts 15 cells from the row's right edge.
    let tabline_test_frame = build_tabline_frame("s", &[("a", true)], None, LockMode::Locked, true);
    let right_block_text = " LOCK · SELECT ";
    assert_eq!(
        format_right_block_text(
            tabline_test_frame
                .render_snapshot
                .build_frame_layout(tabline_test_frame.viewer_chrome)
                .get_tabline_inputs()
        ),
        right_block_text
    );
    assert_eq!(get_text_width(right_block_text), 15);

    let column_count = 30 + VERSION_BADGE_COLUMN_COUNT;
    assert_eq!(
        solve_tabline_layout(&tabline_test_frame, build_tabline_area(column_count),)
            .right_block_start_column,
        column_count - 15
    );

    let render_buffer = render_tabline(&tabline_test_frame, column_count);
    let rendered_text: String = (column_count - 15..column_count)
        .map(|column_index| get_rendered_cell_symbol(&render_buffer, column_index))
        .collect();
    assert_eq!(rendered_text, right_block_text);
}

// --- display-width titles ----------------------------------------------------

#[test]
fn a_wide_cjk_title_counts_two_cells_per_glyph() {
    // " 字 " is 1 + 2 + 1 = 4 cells, so the tab is " #1 "(4) + 4 = 8 wide.
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("字", true)], None, LockMode::Normal, false),
        build_tabline_area(60 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 4 + VERSION_BADGE_COLUMN_COUNT, 8)]
    );
}

#[test]
fn an_emoji_title_counts_two_cells() {
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("🎉", true)], None, LockMode::Normal, false),
        build_tabline_area(60 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 4 + VERSION_BADGE_COLUMN_COUNT, 8)]
    );
}

#[test]
fn a_combining_mark_title_stays_one_cell() {
    // "e" + combining acute is one display cell: " é " is 3, tab is 4 + 3 = 7.
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &[("e\u{0301}", true)], None, LockMode::Normal, false),
        build_tabline_area(60 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 4 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
}

#[test]
fn a_wide_session_name_widens_the_left_block() {
    // " 字 " is 1 + 2 + 1 = 4 cells, one more than " s ", so the strip and its
    // first tab start one cell further right.
    let layout = solve_tabline_layout(
        &build_tabline_frame("字", &[("a", true)], None, LockMode::Normal, false),
        build_tabline_area(20 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.session_block_width, 4 + VERSION_BADGE_COLUMN_COUNT);
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 5 + VERSION_BADGE_COLUMN_COUNT, 7)]
    );
}

#[test]
fn empty_session_and_tab_names_keep_their_padding() {
    // An empty session name is still the 2-cell block "  ", and a tab with an
    // empty name is " #1 "(4) plus "  "(2) — a 6-cell ribbon.
    let frame = build_tabline_frame("", &[("", true)], None, LockMode::Normal, false);
    let layout = solve_tabline_layout(&frame, build_tabline_area(20 + VERSION_BADGE_COLUMN_COUNT));
    assert_eq!(layout.session_block_width, 2 + VERSION_BADGE_COLUMN_COUNT);
    assert_eq!(
        layout.visible_tab_spans,
        vec![build_visible_tab_span(0, 3 + VERSION_BADGE_COLUMN_COUNT, 6)]
    );

    let render_buffer = render_tabline(&frame, 20 + VERSION_BADGE_COLUMN_COUNT);
    let tab_start_column = 3 + VERSION_BADGE_COLUMN_COUNT;
    let rendered_text: String = (tab_start_column..tab_start_column + 6)
        .map(|column_index| get_rendered_cell_symbol(&render_buffer, column_index))
        .collect();
    assert_eq!(rendered_text, " #1   ");
}

#[test]
fn a_two_digit_tab_number_widens_that_tab() {
    // Tab 9 shows "#10" — a wider `#N` block than the single-digit tabs.
    let tab_names_and_activity: Vec<(&str, bool)> =
        (0..10).map(|tab_index| ("a", tab_index == 0)).collect();
    let layout = solve_tabline_layout(
        &build_tabline_frame("s", &tab_names_and_activity, None, LockMode::Normal, false),
        build_tabline_area(200 + VERSION_BADGE_COLUMN_COUNT),
    );
    assert_eq!(layout.visible_tab_spans.len(), 10);
    assert_eq!(layout.visible_tab_spans[8].column_count, 7);
    assert_eq!(layout.visible_tab_spans[9].column_count, 8);
}

// --- painting: exact cells and styles ----------------------------------------

#[test]
fn draw_paints_session_tab_and_mode_with_their_styles() {
    let column_count = 20 + VERSION_BADGE_COLUMN_COUNT;
    let render_buffer = render_tabline(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false),
        column_count,
    );

    // Session block " s " on the left.
    assert_eq!(get_rendered_cell_symbol(&render_buffer, 0), " ");
    assert_eq!(get_rendered_cell_symbol(&render_buffer, 1), "s");
    assert_eq!(get_rendered_cell_symbol(&render_buffer, 2), " ");
    assert_eq!(render_buffer[(1, 0)].fg, Color::Rgb(0xd0, 0xa5, 0xff));
    assert!(render_buffer[(1, 0)].modifier.contains(Modifier::BOLD));

    // Then the version badge `[v…] `: the same ramp color as the name, without
    // its bold. The expected text is spelled out here rather than taken from
    // `create_version_badge_text`, pinning the badge's shape.
    let rendered_badge_text: String = (3..3 + VERSION_BADGE_COLUMN_COUNT)
        .map(|column_index| get_rendered_cell_symbol(&render_buffer, column_index))
        .collect();
    assert_eq!(rendered_badge_text, format!("[v{KOSHI_VERSION}] "));
    assert_eq!(render_buffer[(4, 0)].fg, Color::Rgb(0xd0, 0xa5, 0xff));
    assert!(!render_buffer[(4, 0)].modifier.contains(Modifier::BOLD));

    // One-cell gap after the badge, then the tab " #1  a ".
    let tab_start_column = 4 + VERSION_BADGE_COLUMN_COUNT;
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, tab_start_column - 1),
        " "
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, tab_start_column),
        " "
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, tab_start_column + 1),
        "#"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, tab_start_column + 2),
        "1"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, tab_start_column + 3),
        " "
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, tab_start_column + 4),
        " "
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, tab_start_column + 5),
        "a"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, tab_start_column + 6),
        " "
    );
    // The active tab's `#N` block is its ramp stop as bold text.
    assert_eq!(
        render_buffer[(tab_start_column + 1, 0)].fg,
        Color::Rgb(0xd0, 0xa5, 0xff)
    );
    assert!(render_buffer[(tab_start_column + 1, 0)]
        .modifier
        .contains(Modifier::BOLD));

    // Right block " BASE " anchored to the right edge, its last 6 cells.
    let base_block_start_column = column_count - 6;
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, base_block_start_column),
        " "
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, base_block_start_column + 1),
        "B"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, base_block_start_column + 2),
        "A"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, base_block_start_column + 3),
        "S"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, base_block_start_column + 4),
        "E"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, base_block_start_column + 5),
        " "
    );
    assert_eq!(
        render_buffer[(base_block_start_column + 1, 0)].fg,
        Color::Rgb(0x7d, 0xbc, 0xff)
    );
    assert!(render_buffer[(base_block_start_column + 1, 0)]
        .modifier
        .contains(Modifier::BOLD));
}

#[test]
fn an_inactive_tab_paints_the_dimmed_ramp_as_its_block_background() {
    // Two tabs, the second active. The inactive one is quiet text on the dimmed
    // ramp stop, in no bold; the active one is its ramp stop as text and keeps
    // the bar background, bold on the `#N` block and plain on the name block.
    let render_buffer = render_tabline(
        &build_tabline_frame(
            "s",
            &[("a", false), ("b", true)],
            None,
            LockMode::Normal,
            false,
        ),
        40 + VERSION_BADGE_COLUMN_COUNT,
    );

    let inactive_tab_start_column = 4 + VERSION_BADGE_COLUMN_COUNT;
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, inactive_tab_start_column + 1),
        "#"
    );
    assert_eq!(
        render_buffer[(inactive_tab_start_column + 1, 0)].fg,
        Color::Rgb(0xf0, 0xec, 0xfa)
    );
    assert_eq!(
        render_buffer[(inactive_tab_start_column + 1, 0)].bg,
        Color::Rgb(0x72, 0x5a, 0x8c)
    );
    assert!(!render_buffer[(inactive_tab_start_column + 1, 0)]
        .modifier
        .contains(Modifier::BOLD));
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, inactive_tab_start_column + 5),
        "a"
    );
    assert_eq!(
        render_buffer[(inactive_tab_start_column + 5, 0)].bg,
        Color::Rgb(0x72, 0x5a, 0x8c)
    );

    let active_tab_start_column = 12 + VERSION_BADGE_COLUMN_COUNT;
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, active_tab_start_column + 1),
        "#"
    );
    assert_eq!(
        render_buffer[(active_tab_start_column + 1, 0)].fg,
        Color::Rgb(0x7d, 0xbc, 0xff)
    );
    assert_eq!(
        render_buffer[(active_tab_start_column + 1, 0)].bg,
        Color::Rgb(0x00, 0x00, 0x00)
    );
    assert!(render_buffer[(active_tab_start_column + 1, 0)]
        .modifier
        .contains(Modifier::BOLD));
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, active_tab_start_column + 5),
        "b"
    );
    assert_eq!(
        render_buffer[(active_tab_start_column + 5, 0)].fg,
        Color::Rgb(0x7d, 0xbc, 0xff)
    );
    assert!(!render_buffer[(active_tab_start_column + 5, 0)]
        .modifier
        .contains(Modifier::BOLD));
}

#[test]
fn a_row_below_the_buffer_leaves_every_cell_untouched() {
    // A resize can leave the committed tabline row past the buffer's last row.
    // Nothing is painted there and no cell of the buffer changes.
    let frame = build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false);
    let render_theme = Theme::default();
    let mut render_buffer = Buffer::empty(build_tabline_area(20 + VERSION_BADGE_COLUMN_COUNT));
    render_buffer[(0, 0)].set_symbol("x");
    let buffer_before_paint = render_buffer.clone();

    let below_tabline_area = RatatuiRect {
        x: 0,
        y: 1,
        width: 20 + VERSION_BADGE_COLUMN_COUNT,
        height: 1,
    };
    draw_tabline(
        build_tabline_inputs(&frame),
        &render_theme,
        below_tabline_area,
        &mut render_buffer,
    );
    assert_eq!(render_buffer, buffer_before_paint);
}

#[test]
fn a_row_too_narrow_for_the_badge_drops_it_whole() {
    // 16 cells hold the session block and the " BASE " tag but not the badge
    // as well. The badge is dropped entire rather than cut off part-way. The
    // tab does not fit either, so the strip is just its right arrow.
    let tabline_test_frame =
        build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false);
    let render_buffer = render_tabline(&tabline_test_frame, 16);
    let rendered_row: String = (0..16)
        .map(|column_index| get_rendered_cell_symbol(&render_buffer, column_index))
        .collect();
    assert_eq!(rendered_row, " s       ▶ BASE ");
    assert_eq!(
        solve_tabline_layout(&tabline_test_frame, build_tabline_area(16)).session_block_width,
        3
    );
}

#[test]
fn a_row_narrower_than_the_mode_tag_clips_the_tag_at_the_right_edge() {
    // 3 cells hold neither block whole: the right block starts at column 0 and
    // keeps its first 3 cells, and the session block gets no room at all.
    let tabline_test_frame =
        build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false);
    let layout = solve_tabline_layout(&tabline_test_frame, build_tabline_area(3));
    assert_eq!(layout.session_block_width, 0);
    assert_eq!(layout.right_block_start_column, 0);
    assert!(layout.visible_tab_spans.is_empty());

    let render_buffer = render_tabline(&tabline_test_frame, 3);
    let rendered_row: String = (0..3)
        .map(|column_index| get_rendered_cell_symbol(&render_buffer, column_index))
        .collect();
    assert_eq!(rendered_row, " BA");
}

#[test]
fn draw_fills_the_whole_row_with_the_bar_background() {
    // The session block, badge, and one tab leave the middle empty; every cell
    // of the row still carries the bar background, painted before any text.
    let column_count = 20 + VERSION_BADGE_COLUMN_COUNT;
    let render_buffer = render_tabline(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false),
        column_count,
    );
    for column_index in 0..column_count {
        assert_eq!(
            render_buffer[(column_index, 0)].bg,
            Color::Rgb(0x00, 0x00, 0x00),
            "col {column_index}"
        );
    }
}

#[test]
fn draw_paints_the_select_tag_when_the_mouse_is_grabbed() {
    let column_count = 20 + VERSION_BADGE_COLUMN_COUNT;
    let render_buffer = render_tabline(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, true),
        column_count,
    );
    // " SELECT " fills the row's last 8 cells.
    let mode_tag_start_column = column_count - 8;
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column),
        " "
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 1),
        "S"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 2),
        "E"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 3),
        "L"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 4),
        "E"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 5),
        "C"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 6),
        "T"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 7),
        " "
    );
}

#[test]
fn draw_paints_the_lock_tag_in_locked_mode() {
    let column_count = 20 + VERSION_BADGE_COLUMN_COUNT;
    let render_buffer = render_tabline(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Locked, false),
        column_count,
    );
    // " LOCK " fills the row's last 6 cells.
    let mode_tag_start_column = column_count - 6;
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column),
        " "
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 1),
        "L"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 2),
        "O"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 3),
        "C"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 4),
        "K"
    );
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, mode_tag_start_column + 5),
        " "
    );
}

#[test]
fn draw_paints_the_reconnecting_tag_while_the_viewer_has_no_link() {
    let mut tabline_test_frame =
        build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false);
    tabline_test_frame.viewer_chrome.reconnecting = Some(Reconnecting {
        attempt: 3,
        retry_in_seconds: 8,
    });
    // " RECONNECTING (attempt 3, retry in 8s) " fills the row's last 39 cells,
    // and the row is 16 + VERSION_BADGE_COLUMN_COUNT cells wider than that block.
    let reconnecting_tag_text = " RECONNECTING (attempt 3, retry in 8s) ";
    assert_eq!(
        format_right_block_text(
            tabline_test_frame
                .render_snapshot
                .build_frame_layout(tabline_test_frame.viewer_chrome)
                .get_tabline_inputs()
        ),
        reconnecting_tag_text
    );
    let reconnecting_tag_column_count = get_text_width(reconnecting_tag_text);
    assert_eq!(reconnecting_tag_column_count, 39);
    let column_count = reconnecting_tag_column_count + 16 + VERSION_BADGE_COLUMN_COUNT;
    let render_buffer = render_tabline(&tabline_test_frame, column_count);
    let rendered_text: String = (column_count - reconnecting_tag_column_count..column_count)
        .map(|column_index| get_rendered_cell_symbol(&render_buffer, column_index))
        .collect();
    assert_eq!(rendered_text, reconnecting_tag_text);
}

#[test]
fn draw_paints_the_base_tag_while_the_viewer_has_a_link() {
    let column_count = 30 + VERSION_BADGE_COLUMN_COUNT;
    let tabline_test_frame =
        build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false);
    let render_buffer = render_tabline(&tabline_test_frame, column_count);
    // The same row with the link up ends in the 6-cell " BASE " block.
    let tag_start_column = column_count - 6;
    let rendered_text: String = (tag_start_column..column_count)
        .map(|column_index| get_rendered_cell_symbol(&render_buffer, column_index))
        .collect();
    assert_eq!(rendered_text, " BASE ");
}

#[test]
fn draw_paints_the_right_scroll_arrow_when_a_tab_is_hidden() {
    let column_count = 16 + VERSION_BADGE_COLUMN_COUNT;
    let render_buffer = render_tabline(
        &build_tabline_frame("s", &[("a", true)], None, LockMode::Normal, false),
        column_count,
    );
    // The tab is dropped; a "▶" sits one cell left of the right block.
    let right_arrow_column = column_count - 7;
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, right_arrow_column),
        "▶"
    );
    assert_eq!(
        render_buffer[(right_arrow_column, 0)].fg,
        Color::Rgb(0xf0, 0xec, 0xfa)
    );
    assert!(render_buffer[(right_arrow_column, 0)]
        .modifier
        .contains(Modifier::BOLD));
    // Right block " BASE " still anchors the edge.
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, column_count - 5),
        "B"
    );
}

#[test]
fn draw_paints_the_left_scroll_arrow_when_a_tab_is_hidden_left() {
    let render_buffer = render_tabline(
        &build_tabline_frame(
            "s",
            &[("a", true), ("b", false), ("c", false)],
            Some(1),
            LockMode::Normal,
            false,
        ),
        24 + VERSION_BADGE_COLUMN_COUNT,
    );
    // Peeking from index 1 hides tab 0 off the left: "◀" at the strip start,
    // one cell past the session block and its badge.
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, 4 + VERSION_BADGE_COLUMN_COUNT),
        "◀"
    );
    assert_eq!(
        render_buffer[(4 + VERSION_BADGE_COLUMN_COUNT, 0)].fg,
        Color::Rgb(0xf0, 0xec, 0xfa)
    );
    assert!(render_buffer[(4 + VERSION_BADGE_COLUMN_COUNT, 0)]
        .modifier
        .contains(Modifier::BOLD));
    // And the right arrow marks tab 2 hidden off the right.
    assert_eq!(
        get_rendered_cell_symbol(&render_buffer, 17 + VERSION_BADGE_COLUMN_COUNT),
        "▶"
    );
}

#[test]
fn an_absurdly_long_name_saturates_instead_of_wrapping() {
    // Session and tab names are unbounded strings — a profile file can set one
    // of any length. A name past `u16::MAX` cells measures as `u16::MAX`, which
    // reads as wider than the row rather than wrapping to a small number.
    let oversized_session_name = "x".repeat(usize::from(u16::MAX) + 64);
    assert_eq!(get_text_width(&oversized_session_name), u16::MAX);

    // The solve still answers, and the oversized name claims exactly the 34
    // cells left of the 6-cell " BASE " block — never more than the row.
    let tabline_test_frame = build_tabline_frame(
        &oversized_session_name,
        &[("one", true)],
        None,
        LockMode::Normal,
        false,
    );
    let layout = solve_tabline_layout(
        &tabline_test_frame,
        RatatuiRect {
            x: 0,
            y: 0,
            width: 40,
            height: 1,
        },
    );
    assert_eq!(layout.session_block_width, 34);
    assert_eq!(layout.right_block_start_column, 34);
    assert_eq!(layout.first_visible_tab_index, 0);
    assert!(layout.visible_tab_spans.is_empty());
    assert_eq!(layout.left_scroll_arrow, None);
    assert_eq!(layout.right_scroll_arrow, None);
}

#[test]
fn the_version_badge_text_is_kept_at_exactly_enough_room_and_dropped_one_cell_short() {
    // The badge is all-or-nothing: it fits or it goes whole, never clipped.
    // This pins the `<=` boundary the session block's width is solved from.
    let tabline_test_frame =
        build_tabline_frame("s", &[("one", true)], None, LockMode::Normal, false);
    let tabline_layout = tabline_test_frame
        .render_snapshot
        .build_frame_layout(tabline_test_frame.viewer_chrome);
    let tabline_inputs = tabline_layout.get_tabline_inputs();

    let full_session_block = build_session_block(tabline_inputs.session_name, u16::MAX);
    let session_name_width = get_text_width(&full_session_block.session_name_text);
    let exact_session_block_column_count =
        session_name_width + get_text_width(&create_version_badge_text());

    let session_block_with_badge = build_session_block(
        tabline_inputs.session_name,
        exact_session_block_column_count,
    );
    assert_eq!(session_block_with_badge.session_name_text, " s ");
    assert_eq!(
        session_block_with_badge.version_badge_text,
        Some(create_version_badge_text())
    );
    assert_eq!(
        session_block_with_badge.cell_count,
        exact_session_block_column_count
    );

    let session_block_without_badge = build_session_block(
        tabline_inputs.session_name,
        exact_session_block_column_count - 1,
    );
    assert_eq!(session_block_without_badge.session_name_text, " s ");
    assert_eq!(session_block_without_badge.version_badge_text, None);
    assert_eq!(session_block_without_badge.cell_count, session_name_width);
}

#[test]
fn the_session_block_reports_its_own_width_even_with_no_room_for_it() {
    // With `session_block_room` at 0 the badge goes and the name stays: the
    // block reports the 3 cells " s " needs, and `solve_tabline_layout` is
    // what clamps that to the row.
    let session_block = build_session_block("s", 0);
    assert_eq!(session_block.session_name_text, " s ");
    assert_eq!(session_block.version_badge_text, None);
    assert_eq!(session_block.cell_count, 3);
}
