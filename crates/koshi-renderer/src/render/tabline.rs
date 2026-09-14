//! The tab bar: the session block on the left, the mode tag on the right, and
//! between them the tabs that fit the row with their scroll arrows.

use super::*;

use crate::region::TablineInputs;
use crate::snapshot::TabMeta;

#[cfg(test)]
mod tests;

/// Draw the tabline from [`TablineInputs`] in `theme`'s colors.
/// `tabline_area` is the row to paint into `buffer`.
///
/// The whole row is filled with the theme's bar background (black by default).
/// The session name with the `[v…]` version badge sits on the left and the mode
/// tag on the right, painted as colored text over that fill. Only the tab list
/// between them carries block backgrounds — each tab is a two-block ribbon on
/// its own stop of the theme's chrome ramp (light-purple → light-blue by
/// default). A tab that does not fit is dropped whole, and a `◀` or `▶` marks
/// the side it went off.
///
/// The block widths and per-tab cell spans come from [`solve_tabline_layout`], the
/// same solve [`crate::hit_test()`] reads. A row outside `buf` paints nothing,
/// and a zero-width or zero-height `area` paints nothing.
pub(super) fn draw_tabline(
    tabline_inputs: TablineInputs<'_>,
    theme: &Theme,
    tabline_area: RatatuiRect,
    buffer: &mut Buffer,
) {
    if tabline_area.width == 0 || tabline_area.height == 0 {
        return;
    }
    // Reset every cell of the row, then fill it with the theme's bar
    // background. The session name, badge, mode tag, and arrows set only a
    // foreground, so the fill stays their background; an inactive tab's two
    // blocks set their own background over it.
    Clear.render(tabline_area, buffer);
    buffer.set_style(tabline_area, compute_bar_style(theme));

    let tabline_layout = solve_tabline_layout(tabline_inputs, tabline_area);

    // Right block: it owns the right edge whole.
    let right_block_line = build_right_block(tabline_inputs, theme);
    set_line_clipped(
        buffer,
        tabline_layout.right_block_start_column,
        tabline_area.y,
        &right_block_line,
        tabline_area.right() - tabline_layout.right_block_start_column,
    );

    // Left block: the session name and version badge, measured with the same
    // `session_block_room` the solve used.
    let session_line = build_session_line(
        tabline_inputs.session_name,
        theme,
        tabline_layout
            .right_block_start_column
            .saturating_sub(tabline_area.x),
    );
    set_line_clipped(
        buffer,
        tabline_area.x,
        tabline_area.y,
        &session_line,
        tabline_layout.session_block_width,
    );

    // Tab ribbons in the windowed middle, each on its own ramp stop.
    for visible_tab_span in &tabline_layout.visible_tab_spans {
        let tab_line = build_tab_line(
            tabline_inputs.tabs_metadata,
            theme,
            visible_tab_span.tab_metadata_index,
        );
        set_line_clipped(
            buffer,
            visible_tab_span.start_column,
            tabline_area.y,
            &tab_line,
            visible_tab_span.column_count,
        );
    }
    // A `◀`/`▶` on each side that still hides tabs.
    if let Some(left_scroll_arrow) = tabline_layout.left_scroll_arrow {
        let left_arrow_line = Line::from(Span::styled("◀", compute_scroll_arrow_style(theme)));
        set_line_clipped(
            buffer,
            left_scroll_arrow.start_column,
            tabline_area.y,
            &left_arrow_line,
            TABLINE_ARROW_WIDTH,
        );
    }
    if let Some(right_scroll_arrow) = tabline_layout.right_scroll_arrow {
        let right_arrow_line = Line::from(Span::styled("▶", compute_scroll_arrow_style(theme)));
        set_line_clipped(
            buffer,
            right_scroll_arrow.start_column,
            tabline_area.y,
            &right_arrow_line,
            TABLINE_ARROW_WIDTH,
        );
    }
}

/// The one-cell width a tabline scroll arrow reserves and occupies.
pub(crate) const TABLINE_ARROW_WIDTH: u16 = 1;

/// The tabline's solved geometry for one frame: where the two anchored blocks
/// sit, the windowed run of visible tabs, and the scroll arrows framing it.
///
/// [`draw_tabline`] paints from it, and [`crate::hit_test()`] maps a click to a
/// tab or arrow with it. Both read the same solve.
pub(crate) struct TablineLayout {
    /// Cells the left session block occupies, measured from `tabline_area.x`.
    pub session_block_width: u16,
    /// The column where the right block (scroll + mode tag) starts.
    pub right_block_start_column: u16,
    /// The metadata index the visible window starts at. When no tab fits, it
    /// is still the index the window starts from.
    pub first_visible_tab_index: usize,
    /// The visible tab spans in left-to-right order.
    pub visible_tab_spans: Vec<VisibleTabSpan>,
    /// The left scroll arrow when tabs are hidden off the left.
    pub left_scroll_arrow: Option<TablineScrollArrow>,
    /// The right scroll arrow when tabs are hidden off the right.
    pub right_scroll_arrow: Option<TablineScrollArrow>,
}

/// The columns occupied by one visible tab ribbon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VisibleTabSpan {
    pub tab_metadata_index: usize,
    pub start_column: u16,
    pub column_count: u16,
}

/// The position and target of one tabline scroll arrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TablineScrollArrow {
    pub start_column: u16,
    pub target_first_visible_tab_index: usize,
}

/// Solve the tabline's block widths, its windowed run of tabs, and its scroll
/// arrows for `tabline_area`.
///
/// The right block anchors the right edge and the session block the left. If
/// every tab fits in the gap between them, all are shown from index 0 with no
/// arrows. Otherwise the strip scrolls, a one-cell arrow is reserved on each
/// side and drawn on whichever side still hides tabs, and the window starts at:
///
/// - the client's
///   [`tabline_offset`](crate::snapshot::ViewerChrome::tabline_offset) when it
///   is peeking, clamped to the last tab;
/// - otherwise the smallest index that keeps the active tab on screen, which is
///   the active tab's own index when that tab is wider than the window — a
///   window holding no tab at all.
///
/// The first tab marked active is the one followed, and index 0 when none is
/// marked. A row leaving no gap between the two blocks yields no tabs and no
/// arrows.
pub(crate) fn solve_tabline_layout(
    tabline_inputs: TablineInputs<'_>,
    tabline_area: RatatuiRect,
) -> TablineLayout {
    let right_block_width = get_text_width(&format_right_block_text(tabline_inputs));
    let right_block_start_column = tabline_area
        .right()
        .saturating_sub(right_block_width)
        .max(tabline_area.x);
    let session_block_room = right_block_start_column.saturating_sub(tabline_area.x);
    let session_block = build_session_block(tabline_inputs.session_name, session_block_room);
    let session_block_width = session_block.cell_count.min(session_block_room);
    let tab_strip_start_column = tabline_area
        .x
        .saturating_add(session_block_width)
        .saturating_add(1);

    let tab_count = tabline_inputs.tabs_metadata.len();
    let create_empty_layout = || TablineLayout {
        session_block_width,
        right_block_start_column,
        first_visible_tab_index: 0,
        visible_tab_spans: Vec::new(),
        left_scroll_arrow: None,
        right_scroll_arrow: None,
    };
    if tab_count == 0 || tab_strip_start_column >= right_block_start_column {
        return create_empty_layout();
    }

    let tab_column_counts: Vec<u16> = (0..tab_count)
        .map(|tab_metadata_index| {
            let (tab_index_text, tab_name_text) =
                get_tab_text_blocks(tabline_inputs.tabs_metadata, tab_metadata_index);
            get_text_width(&tab_index_text).saturating_add(get_text_width(&tab_name_text))
        })
        .collect();

    // Everything fits from the first tab: show them all, no scrolling.
    let unscrolled_visible_tab_spans = pack_visible_tabs(
        &tab_column_counts,
        0,
        tab_strip_start_column,
        right_block_start_column,
    );
    if unscrolled_visible_tab_spans.len() == tab_count {
        return TablineLayout {
            session_block_width,
            right_block_start_column,
            first_visible_tab_index: 0,
            visible_tab_spans: unscrolled_visible_tab_spans,
            left_scroll_arrow: None,
            right_scroll_arrow: None,
        };
    }

    // Scrolled: reserve one arrow cell on each side. A reserved cell that
    // draws no arrow (nothing hidden that side) stays a one-cell gap.
    let visible_tabs_start_column = tab_strip_start_column.saturating_add(TABLINE_ARROW_WIDTH);
    let visible_tabs_end_column = right_block_start_column.saturating_sub(TABLINE_ARROW_WIDTH);
    if visible_tabs_start_column >= visible_tabs_end_column {
        return create_empty_layout();
    }

    let active_tab_index = tabline_inputs
        .tabs_metadata
        .iter()
        .position(|tab_metadata| tab_metadata.is_active)
        .unwrap_or(0);
    let first_visible_tab_index = match tabline_inputs.tabline_offset {
        Some(peeked_tab_index) => peeked_tab_index.min(tab_count - 1),
        None => find_first_visible_tab_index(
            &tab_column_counts,
            active_tab_index,
            visible_tabs_start_column,
            visible_tabs_end_column,
        ),
    };

    let visible_tab_spans = pack_visible_tabs(
        &tab_column_counts,
        first_visible_tab_index,
        visible_tabs_start_column,
        visible_tabs_end_column,
    );
    let after_window_tab_index = first_visible_tab_index + visible_tab_spans.len();
    let left_scroll_arrow = (first_visible_tab_index > 0).then(|| TablineScrollArrow {
        start_column: tab_strip_start_column,
        target_first_visible_tab_index: first_visible_tab_index - 1,
    });
    let right_scroll_arrow = (after_window_tab_index < tab_count).then(|| TablineScrollArrow {
        start_column: right_block_start_column - TABLINE_ARROW_WIDTH,
        target_first_visible_tab_index: first_visible_tab_index + 1,
    });

    TablineLayout {
        session_block_width,
        right_block_start_column,
        first_visible_tab_index,
        visible_tab_spans,
        left_scroll_arrow,
        right_scroll_arrow,
    }
}

/// Place tabs from index `first_visible_tab_index` into the half-open column
/// range `[visible_tabs_start_column, visible_tabs_end_column)` with a one-cell
/// gap between them, stopping at the first tab that would not fit.
fn pack_visible_tabs(
    tab_column_counts: &[u16],
    first_visible_tab_index: usize,
    visible_tabs_start_column: u16,
    visible_tabs_end_column: u16,
) -> Vec<VisibleTabSpan> {
    let mut visible_tab_spans = Vec::new();
    let mut start_column = visible_tabs_start_column;
    for (tab_metadata_index, &column_count) in tab_column_counts
        .iter()
        .enumerate()
        .skip(first_visible_tab_index)
    {
        if u32::from(start_column) + u32::from(column_count) > u32::from(visible_tabs_end_column) {
            break;
        }
        visible_tab_spans.push(VisibleTabSpan {
            tab_metadata_index,
            start_column,
            column_count,
        });
        start_column = start_column.saturating_add(column_count).saturating_add(1);
    }
    visible_tab_spans
}

/// The smallest first-visible index that keeps `active_tab_index` on screen
/// when packing into `[visible_tabs_start_column, visible_tabs_end_column)`.
/// `0` is returned when the active tab fits from the left, and
/// `active_tab_index` when that tab is wider than the window.
fn find_first_visible_tab_index(
    tab_column_counts: &[u16],
    active_tab_index: usize,
    visible_tabs_start_column: u16,
    visible_tabs_end_column: u16,
) -> usize {
    let is_active_tab_visible = |first_visible_tab_index: usize| {
        pack_visible_tabs(
            tab_column_counts,
            first_visible_tab_index,
            visible_tabs_start_column,
            visible_tabs_end_column,
        )
        .iter()
        .any(|visible_tab_span| visible_tab_span.tab_metadata_index == active_tab_index)
    };
    if is_active_tab_visible(0) {
        return 0;
    }
    let mut first_visible_tab_index = active_tab_index;
    while first_visible_tab_index > 0 && is_active_tab_visible(first_visible_tab_index - 1) {
        first_visible_tab_index -= 1;
    }
    first_visible_tab_index
}

/// The tabline's right-anchored block text: the tag [`build_mode_tags`] composes,
/// with a space each side. A plain client gives ` BASE `; one that is locked,
/// selecting, and dialing the session again gives
/// ` RECONNECTING (attempt 4, retry in 8s) · LOCK · SELECT `.
fn format_right_block_text(tabline_inputs: TablineInputs<'_>) -> String {
    format!(
        " {} ",
        build_mode_tags(
            tabline_inputs.lock_mode,
            tabline_inputs.is_mouse_selection_enabled,
            tabline_inputs.reconnecting,
        )
    )
}

/// The right-anchored block, colored for drawing.
fn build_right_block(tabline_inputs: TablineInputs<'_>, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format_right_block_text(tabline_inputs),
        compute_mode_style(theme),
    ))
}

/// koshi's own version, shown as the `[v…]` badge beside the session name.
/// This crate's `CARGO_PKG_VERSION`, which the workspace sets for every koshi
/// crate.
const KOSHI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The `[v…] ` badge text the tabline paints, trailing space included.
pub(crate) fn create_version_badge_text() -> String {
    format!("[v{KOSHI_VERSION}] ")
}

/// The tabline's left-anchored block text: the session name with a space each
/// side, then the `[v…] ` badge from [`create_version_badge_text`]. Session `my-session`
/// gives ` my-session ` followed by that badge.
///
/// `session_block_room` is the cells the block has before the right-anchored
/// mode tag. When both parts do not fit in `session_block_room`, the badge is
/// dropped whole: a 16-cell row
/// ending in the 6-cell ` BASE ` tag shows ` s ` alone, never a badge cut off
/// part-way. The returned width is not clipped to `session_block_room`; a name
/// wider than `session_block_room` carries its own full width out.
fn build_session_block(session_name: &str, session_block_room: u16) -> SessionBlock {
    let session_name_text = format!(" {} ", session_name);
    let version_badge_text = create_version_badge_text();
    let session_name_width = get_text_width(&session_name_text);
    let version_badge_width = get_text_width(&version_badge_text);
    let combined_width = session_name_width.saturating_add(version_badge_width);
    if combined_width <= session_block_room {
        SessionBlock {
            cell_count: combined_width,
            session_name_text,
            version_badge_text: Some(version_badge_text),
        }
    } else {
        SessionBlock {
            cell_count: session_name_width,
            session_name_text,
            version_badge_text: None,
        }
    }
}

/// The left block's text and the cells it occupies, measured once by
/// [`build_session_block`].
struct SessionBlock {
    /// The session name with a space each side. Always drawn.
    session_name_text: String,
    /// The version badge, present only when it fit whole beside the name.
    version_badge_text: Option<String>,
    /// Cells `session_name_text` and `version_badge_text` occupy together, or
    /// `session_name_text` alone when there is no badge.
    cell_count: u16,
}

/// The left-anchored block, colored for drawing. `session_block_room` is the
/// cells before the mode tag, as in [`build_session_block`], and decides
/// whether the badge is there.
fn build_session_line(session_name: &str, theme: &Theme, session_block_room: u16) -> Line<'static> {
    let session_block = build_session_block(session_name, session_block_room);
    let session_name_span = Span::styled(
        session_block.session_name_text,
        compute_session_style(theme),
    );
    match session_block.version_badge_text {
        Some(version_badge_text) => Line::from(vec![
            session_name_span,
            Span::styled(version_badge_text, compute_version_badge_style(theme)),
        ]),
        None => Line::from(session_name_span),
    }
}

/// One tab's two text blocks at metadata index `tab_metadata_index`: ` #N `,
/// where `N` is the tab's own `tab_index` field plus one, and its name with a
/// space each side. A tab with `tab_index: 0` and `tab_name: "shell"` gives
/// `(" #1 ", " shell ")`.
///
/// # Panics
///
/// Panics when `tab_metadata_index` is not an index of `tabs_metadata`.
fn get_tab_text_blocks(tabs_metadata: &[TabMeta], tab_metadata_index: usize) -> (String, String) {
    let tab_metadata = &tabs_metadata[tab_metadata_index];
    (
        format!(" #{} ", tab_metadata.tab_index + 1),
        format!(" {} ", tab_metadata.tab_name),
    )
}

/// One tab's two-block ribbon (`#N` block + name block) at metadata index
/// `tab_metadata_index`, colored on its own stop of the theme's chrome ramp.
///
/// # Panics
///
/// Panics when `tab_metadata_index` is not an index of `tabs_metadata`.
fn build_tab_line(
    tabs_metadata: &[TabMeta],
    theme: &Theme,
    tab_metadata_index: usize,
) -> Line<'static> {
    let tab_count = tabs_metadata.len();
    let is_active = tabs_metadata[tab_metadata_index].is_active;
    let (tab_index_text, tab_name_text) = get_tab_text_blocks(tabs_metadata, tab_metadata_index);
    Line::from(vec![
        Span::styled(
            tab_index_text,
            compute_tab_index_style(theme, is_active, tab_metadata_index, tab_count),
        ),
        Span::styled(
            tab_name_text,
            compute_tab_name_style(theme, is_active, tab_metadata_index, tab_count),
        ),
    ])
}
