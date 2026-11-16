//! The tab bar: the session block on the left, the mode tag on the right, the
//! floating pane tray button left of the mode tag, the notification button
//! left of the tray button, the client chips left of the buttons while another
//! client is attached, and between them the tabs that fit the row with their
//! scroll arrows.

use std::ops::Range;

use ratatui::widgets::{Clear, Widget};

use super::*;

use koshi_core::client::compute_client_number;

use crate::notification::{find_notification_button_columns, format_notification_button_text};
use crate::region::TablineInputs;
use crate::snapshot::TabMetadata;
use crate::theme::ClientColor;

#[cfg(test)]
mod tests;

/// Draw the tabline from [`TablineInputs`] in `theme`'s colors.
/// `tabline_area` is the row to paint into `screen_buffer`.
///
/// The whole row is filled with the theme's bar background (black by default).
/// The session name with the `[v…]` version badge sits on the left and the mode
/// tag on the right, painted as colored text over that fill. The tray button
/// ` ⧉ ` sits left of the mode tag in the mode tag's style, in reverse video
/// while the lock mode is `FloatingPaneTray`. The notification button ` ● ` or
/// ` ●3 ` sits left of the tray button in the mode tag's style, in reverse
/// video while the lock mode is `NotificationCenter`. A center that slides out
/// draws the button with no reverse video. A button that the row has no room
/// for is not drawn, and the notification button is not drawn with no tray
/// button. While another client is attached, the client chips sit left of the
/// buttons: each chip is ` 3 ` in its client's colors
/// ([`Theme::compute_client_color`](crate::theme::Theme::compute_client_color)),
/// the viewing client's own chip first with its number bold and underlined,
/// then `+N` in the scroll arrows' style when N clients have no chip.
/// Only the tab list between them carries block backgrounds — each tab is a
/// two-block ribbon on its own stop of the theme's chrome ramp (light-purple →
/// light-blue by default). A tab that does not fit is dropped whole, and a `◀`
/// or `▶` marks the side it went off.
///
/// The block widths and per-tab cell spans come from [`solve_tabline_layout`], the
/// same solve [`crate::resolve_hit_region()`] reads. A row outside `screen_buffer` paints
/// nothing, and a zero-width or zero-height `tabline_area` paints nothing.
pub(super) fn draw_tabline(
    tabline_inputs: TablineInputs<'_>,
    theme: &Theme,
    tabline_area: RatatuiRect,
    screen_buffer: &mut Buffer,
) {
    if tabline_area.width == 0 || tabline_area.height == 0 {
        return;
    }
    // Reset every cell of the row, then fill it with the theme's bar
    // background. The session name, badge, mode tag, and arrows set only a
    // foreground, so the fill stays their background; an inactive tab's two
    // blocks set their own background over it.
    Clear.render(tabline_area, screen_buffer);
    screen_buffer.set_style(tabline_area, compute_bar_style(theme));

    let tabline_layout = solve_tabline_layout(tabline_inputs, tabline_area);

    // Right block: it owns the right edge whole.
    let right_block_line = build_right_block(tabline_inputs, theme);
    set_line_clipped(
        screen_buffer,
        tabline_layout.right_block_start_column,
        tabline_area.y,
        &right_block_line,
        tabline_area.right() - tabline_layout.right_block_start_column,
    );

    if let Some(floating_pane_tray_button_column) = tabline_layout.floating_pane_tray_button_column
    {
        let mut button_style = compute_mode_style(theme);
        if tabline_inputs.lock_mode == LockMode::FloatingPaneTray {
            button_style = button_style.add_modifier(Modifier::REVERSED);
        }
        set_line_clipped(
            screen_buffer,
            floating_pane_tray_button_column,
            tabline_area.y,
            &Line::from(Span::styled(FLOATING_PANE_TRAY_BUTTON_TEXT, button_style)),
            FLOATING_PANE_TRAY_BUTTON_COLUMN_COUNT,
        );
    }
    for client_chip_span in &tabline_layout.client_chip_spans {
        set_line_clipped(
            screen_buffer,
            client_chip_span.start_column,
            tabline_area.y,
            &build_client_chip_line(
                *client_chip_span,
                theme.compute_client_color(client_chip_span.color_index),
            ),
            tabline_area.right() - client_chip_span.start_column,
        );
    }
    if let Some(hidden_client_mark) = tabline_layout.hidden_client_mark {
        set_line_clipped(
            screen_buffer,
            hidden_client_mark.start_column,
            tabline_area.y,
            &Line::from(Span::styled(
                format!("+{}", hidden_client_mark.client_count),
                compute_scroll_arrow_style(theme),
            )),
            tabline_area.right() - hidden_client_mark.start_column,
        );
    }
    if let Some(notification_button_columns) = &tabline_layout.notification_button_columns {
        let mut button_style = compute_mode_style(theme);
        if tabline_inputs.lock_mode == LockMode::NotificationCenter {
            button_style = button_style.add_modifier(Modifier::REVERSED);
        }
        set_line_clipped(
            screen_buffer,
            notification_button_columns.start,
            tabline_area.y,
            &Line::from(Span::styled(
                format_notification_button_text(tabline_inputs.notification_button),
                button_style,
            )),
            notification_button_columns.end - notification_button_columns.start,
        );
    }

    // Left block: the session name and version badge, measured with the same
    // `session_block_room` the solve used.
    let session_line = build_session_line(
        tabline_inputs.session_name,
        theme,
        tabline_layout
            .tab_strip_end_column
            .saturating_sub(tabline_area.x),
    );
    set_line_clipped(
        screen_buffer,
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
            screen_buffer,
            visible_tab_span.start_column,
            tabline_area.y,
            &tab_line,
            visible_tab_span.column_count,
        );
    }
    // A `◀`/`▶` on each side that still hides tabs.
    if let Some(left_scroll_arrow_column) = tabline_layout.left_scroll_arrow_column {
        let left_arrow_line = Line::from(Span::styled("◀", compute_scroll_arrow_style(theme)));
        set_line_clipped(
            screen_buffer,
            left_scroll_arrow_column,
            tabline_area.y,
            &left_arrow_line,
            TABLINE_ARROW_WIDTH,
        );
    }
    if let Some(right_scroll_arrow_column) = tabline_layout.right_scroll_arrow_column {
        let right_arrow_line = Line::from(Span::styled("▶", compute_scroll_arrow_style(theme)));
        set_line_clipped(
            screen_buffer,
            right_scroll_arrow_column,
            tabline_area.y,
            &right_arrow_line,
            TABLINE_ARROW_WIDTH,
        );
    }
}

/// The one-cell width a tabline scroll arrow reserves and occupies.
pub(crate) const TABLINE_ARROW_WIDTH: u16 = 1;

/// The tabline's floating pane tray button: the glyph `⧉` with a space each
/// side. `⧉` (`U+29C9`) is one cell wide, also where East Asian ambiguous
/// characters take two.
pub(crate) const FLOATING_PANE_TRAY_BUTTON_TEXT: &str = " ⧉ ";

/// The cells [`FLOATING_PANE_TRAY_BUTTON_TEXT`] takes.
pub(crate) const FLOATING_PANE_TRAY_BUTTON_COLUMN_COUNT: u16 = 3;

/// The tabline's solved geometry for one frame: where the two anchored blocks,
/// the tray button, the notification button and the client chips sit, the
/// windowed run of visible tabs, and the scroll arrows framing it.
///
/// [`draw_tabline`] paints from it, and [`crate::resolve_hit_region()`] maps a click to a
/// tab or arrow with it. Both read the same solve.
pub(crate) struct TablineLayout {
    /// Cells the left session block occupies, measured from `tabline_area.x`.
    pub session_block_width: u16,
    /// The column where the right block, the mode tag, starts.
    pub right_block_start_column: u16,
    /// The column where the tray button ` ⧉ ` starts, three cells left of the
    /// mode tag. `None` when the row leaves fewer than three cells left of the
    /// mode tag.
    pub floating_pane_tray_button_column: Option<u16>,
    /// The columns the notification button takes, right before the tray
    /// button. `None` with no tray button, or when the row leaves no room for
    /// the button.
    pub notification_button_columns: Option<Range<u16>>,
    /// The client chips, left to right: the viewing client's own chip, then
    /// one chip per peer in `color_index` order. Empty while the viewing
    /// client is the only attached client, and when the viewing client's own
    /// chip does not fit.
    pub client_chip_spans: Vec<ClientChipSpan>,
    /// The `+N` mark right of the client chips. `None` while every attached
    /// client has a chip, and when the row has no room for the mark.
    pub hidden_client_mark: Option<HiddenClientMark>,
    /// The column where the session block and the tab strip end: one blank
    /// cell before the first client chip or the `+N` mark, else the
    /// notification button's first column, else the tray button's column,
    /// else the mode tag's column.
    pub tab_strip_end_column: u16,
    /// The metadata index the visible window starts at. When no tab fits, it
    /// is still the index the window starts from.
    pub first_visible_tab_index: usize,
    /// The visible tab spans in left-to-right order.
    pub visible_tab_spans: Vec<VisibleTabSpan>,
    /// The column of the left scroll arrow, or `None` when no tab is hidden off
    /// the left.
    pub left_scroll_arrow_column: Option<u16>,
    /// The column of the right scroll arrow, or `None` when no tab is hidden
    /// off the right.
    pub right_scroll_arrow_column: Option<u16>,
}

/// One client chip of the tab strip: ` 3 `, the client number with one
/// blank cell on each side. The number is `color_index + 1`
/// ([`compute_client_number`]), so index `11` draws ` 12 ` in four cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClientChipSpan {
    /// The index of the client the chip names.
    pub color_index: u32,
    /// Whether the chip names the viewing client, which draws its number bold
    /// and underlined.
    pub is_viewer_chip: bool,
    /// The column of the chip's first cell.
    pub start_column: u16,
}

/// The `+N` mark right of the client chips: `N` attached clients, the
/// viewing client included, have no chip drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HiddenClientMark {
    /// The column of the `+`.
    pub start_column: u16,
    /// The count of attached clients with no chip drawn, at least `1`.
    pub client_count: usize,
}

/// The columns occupied by one visible tab ribbon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VisibleTabSpan {
    pub tab_metadata_index: usize,
    pub start_column: u16,
    pub column_count: u16,
}

/// Solve the tabline's block widths, its windowed run of tabs, and its scroll
/// arrows for `tabline_area`.
///
/// The right block anchors the right edge and the session block the left.
/// The tray button takes the three cells left of the right block when the row
/// has them. The notification button takes the cells left of the tray button
/// when the row has them. The client chips take the cells left of the
/// leftmost button ([`solve_client_chips`]), and one blank cell separates
/// them from the gap. The gap ends there, or at the leftmost button with no
/// chip drawn. If every tab fits in the gap, all are shown from index 0
/// with no arrows. Otherwise the strip scrolls, a one-cell arrow is reserved on
/// each side and drawn on whichever side still hides tabs, and the window
/// starts at:
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
    let right_block_start_column = compute_right_block_start_column(tabline_inputs, tabline_area);
    let floating_pane_tray_button_column =
        find_floating_pane_tray_button_column(tabline_area, right_block_start_column);
    let notification_button_columns =
        floating_pane_tray_button_column.and_then(|floating_pane_tray_button_column| {
            find_notification_button_columns(
                tabline_inputs.notification_button,
                tabline_area,
                floating_pane_tray_button_column,
            )
        });
    let leftmost_button_column = notification_button_columns
        .as_ref()
        .map(|notification_button_columns| notification_button_columns.start)
        .or(floating_pane_tray_button_column)
        .unwrap_or(right_block_start_column);
    let (client_chip_spans, hidden_client_mark, client_chip_group_start_column) =
        solve_client_chips(tabline_inputs, tabline_area.x, leftmost_button_column);
    let tab_strip_end_column = match client_chip_group_start_column {
        Some(group_start_column) => group_start_column.saturating_sub(1),
        None => leftmost_button_column,
    };
    let session_block_room = tab_strip_end_column.saturating_sub(tabline_area.x);
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
        floating_pane_tray_button_column,
        notification_button_columns: notification_button_columns.clone(),
        client_chip_spans: client_chip_spans.clone(),
        hidden_client_mark,
        tab_strip_end_column,
        first_visible_tab_index: 0,
        visible_tab_spans: Vec::new(),
        left_scroll_arrow_column: None,
        right_scroll_arrow_column: None,
    };
    if tab_count == 0 || tab_strip_start_column >= tab_strip_end_column {
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
        tab_strip_end_column,
    );
    if unscrolled_visible_tab_spans.len() == tab_count {
        return TablineLayout {
            session_block_width,
            right_block_start_column,
            floating_pane_tray_button_column,
            notification_button_columns,
            client_chip_spans,
            hidden_client_mark,
            tab_strip_end_column,
            first_visible_tab_index: 0,
            visible_tab_spans: unscrolled_visible_tab_spans,
            left_scroll_arrow_column: None,
            right_scroll_arrow_column: None,
        };
    }

    // Scrolled: reserve one arrow cell on each side. A reserved cell that
    // draws no arrow (nothing hidden that side) stays a one-cell gap.
    let visible_tabs_start_column = tab_strip_start_column.saturating_add(TABLINE_ARROW_WIDTH);
    let visible_tabs_end_column = tab_strip_end_column.saturating_sub(TABLINE_ARROW_WIDTH);
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
    let left_scroll_arrow_column = (first_visible_tab_index > 0).then_some(tab_strip_start_column);
    let right_scroll_arrow_column =
        (after_window_tab_index < tab_count).then_some(tab_strip_end_column - TABLINE_ARROW_WIDTH);

    TablineLayout {
        session_block_width,
        right_block_start_column,
        floating_pane_tray_button_column,
        notification_button_columns,
        client_chip_spans,
        hidden_client_mark,
        tab_strip_end_column,
        first_visible_tab_index,
        visible_tab_spans,
        left_scroll_arrow_column,
        right_scroll_arrow_column,
    }
}

/// The client chips and the `+N` mark of the tab strip, right-aligned so the
/// last one ends at `group_end_column`, and the column where the first one
/// starts. `tabline_area_column` is the row's first column. Nothing, and
/// `None`, while `tabline_inputs.peers` is empty.
///
/// The chips are the viewing client's own, then one per peer in list order.
/// One blank cell separates two chips, and a chip and the mark. The group
/// takes at most half of the cells from `tabline_area_column` up to
/// `group_end_column`, rounded down:
///
/// 1. When every chip fits, every chip is drawn and no mark.
/// 2. Otherwise the mark `+N` takes `+` and the digits of the attached
///    client count, the viewing client included. The chips that fit before
///    the mark are drawn, stopping at the first that does not, and `N` counts
///    the rest. When the mark alone does not fit, nothing is drawn.
///
/// Example: with 12 cells of half room, a viewer of index `0` and peers of
/// indexes `1` to `4` draw ` 1 ` and ` 2 ` and `+3`: ` 1   2  +3`.
fn solve_client_chips(
    tabline_inputs: TablineInputs<'_>,
    tabline_area_column: u16,
    group_end_column: u16,
) -> (Vec<ClientChipSpan>, Option<HiddenClientMark>, Option<u16>) {
    if tabline_inputs.peers.is_empty() {
        return (Vec::new(), None, None);
    }
    let group_room = group_end_column.saturating_sub(tabline_area_column) / 2;
    let list_chip_clients = || {
        std::iter::once((tabline_inputs.viewer_color_index, true)).chain(
            tabline_inputs
                .peers
                .iter()
                .map(|peer_snapshot| (peer_snapshot.color_index, false)),
        )
    };
    let client_count = tabline_inputs.peers.len() + 1;
    let (mut packed_chips, mut chips_width) = pack_client_chips(list_chip_clients(), group_room);
    let mut hidden_client_mark_width = 0;
    if packed_chips.len() < client_count {
        hidden_client_mark_width = 1 + count_decimal_digits(client_count as u64);
        if hidden_client_mark_width > group_room {
            return (Vec::new(), None, None);
        }
        (packed_chips, chips_width) = pack_client_chips(
            list_chip_clients(),
            group_room.saturating_sub(hidden_client_mark_width + 1),
        );
    }
    let separator_width = if !packed_chips.is_empty() && hidden_client_mark_width > 0 {
        1
    } else {
        0
    };
    let group_width = chips_width + separator_width + hidden_client_mark_width;
    let group_start_column = group_end_column - group_width;
    let hidden_client_mark = (hidden_client_mark_width > 0).then(|| HiddenClientMark {
        start_column: group_start_column + chips_width + separator_width,
        client_count: client_count - packed_chips.len(),
    });
    let client_chip_spans = packed_chips
        .into_iter()
        .map(
            |(color_index, is_viewer_chip, chip_offset)| ClientChipSpan {
                color_index,
                is_viewer_chip,
                start_column: group_start_column + chip_offset,
            },
        )
        .collect();
    (
        client_chip_spans,
        hidden_client_mark,
        Some(group_start_column),
    )
}

/// The chips of `chip_clients`, each `(color_index, is_viewer_chip)`, that
/// fit `room_width` cells in order, with one blank cell between two chips,
/// stopping at the first chip that does not fit: each with its offset from the
/// first chip's column, and the cells they take together. Chips of indexes
/// `0`, `9` and `10` in 9 cells give offsets `0` and `4` and width `8`.
fn pack_client_chips(
    chip_clients: impl Iterator<Item = (u32, bool)>,
    room_width: u16,
) -> (Vec<(u32, bool, u16)>, u16) {
    let mut packed_chips = Vec::new();
    let mut chips_width: u16 = 0;
    for (color_index, is_viewer_chip) in chip_clients {
        let chip_offset = if packed_chips.is_empty() {
            0
        } else {
            chips_width + 1
        };
        let chip_end_offset = chip_offset + compute_client_chip_width(color_index);
        if chip_end_offset > room_width {
            break;
        }
        packed_chips.push((color_index, is_viewer_chip, chip_offset));
        chips_width = chip_end_offset;
    }
    (packed_chips, chips_width)
}

/// The cells the chip of the client of index `color_index` takes: the digits
/// of its number ([`compute_client_number`]) and one blank cell on each side.
/// Index `0` gives `3`, index `11` gives `4`, and index `4294967295`
/// gives `12`.
fn compute_client_chip_width(color_index: u32) -> u16 {
    count_decimal_digits(compute_client_number(color_index)) + 2
}

/// The digits `value` takes in decimal: `0` and `9` give `1`, `10` gives `2`,
/// and `u64::MAX` gives `20`.
fn count_decimal_digits(value: u64) -> u16 {
    value
        .checked_ilog10()
        .map_or(1, |decimal_log| decimal_log as u16 + 1)
}

/// The chip line of `client_chip_span` in `client_color`: the client number
/// with one blank cell on each side, its number bold and underlined when the
/// chip names the viewing client. Index `2` gives ` 3 `.
fn build_client_chip_line(
    client_chip_span: ClientChipSpan,
    client_color: ClientColor,
) -> Line<'static> {
    let chip_style = compute_client_color_style(client_color);
    let client_number_text = compute_client_number(client_chip_span.color_index).to_string();
    let client_number_style = if client_chip_span.is_viewer_chip {
        chip_style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        chip_style
    };
    Line::from(vec![
        Span::styled(" ", chip_style),
        Span::styled(client_number_text, client_number_style),
        Span::styled(" ", chip_style),
    ])
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

/// The column where the right block starts on the row `tabline_area`: the
/// row's end minus the width of [`format_right_block_text`], and the row's
/// first column at least. An `80`-column row ending in ` BASE ` gives `74`.
pub(crate) fn compute_right_block_start_column(
    tabline_inputs: TablineInputs<'_>,
    tabline_area: RatatuiRect,
) -> u16 {
    let right_block_width = get_text_width(&format_right_block_text(tabline_inputs));
    tabline_area
        .right()
        .saturating_sub(right_block_width)
        .max(tabline_area.x)
}

/// The column where the tray button ` ⧉ ` starts: the three cells left of
/// `right_block_start_column`. `None` when fewer than three cells of
/// `tabline_area` lie left of `right_block_start_column`. A right block at
/// column `74` gives `71`.
pub(crate) fn find_floating_pane_tray_button_column(
    tabline_area: RatatuiRect,
    right_block_start_column: u16,
) -> Option<u16> {
    let is_button_drawn = right_block_start_column.saturating_sub(tabline_area.x)
        >= FLOATING_PANE_TRAY_BUTTON_COLUMN_COUNT;
    is_button_drawn.then(|| right_block_start_column - FLOATING_PANE_TRAY_BUTTON_COLUMN_COUNT)
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
pub(crate) fn format_version_badge_text() -> String {
    format!("[v{KOSHI_VERSION}] ")
}

/// The tabline's left-anchored block text: the session name with a space each
/// side, then the `[v…] ` badge from [`format_version_badge_text`]. Session `my-session`
/// gives ` my-session ` followed by that badge.
///
/// `session_block_room` is the cells the block has before the tab strip's end
/// column: the notification button, else the tray button, else the
/// right-anchored mode tag. When both parts do
/// not fit in `session_block_room`, the badge is dropped whole: a 16-cell row
/// ending in the 6-cell ` BASE ` tag shows ` s ` alone, never a badge cut off
/// part-way. The returned width is not clipped to `session_block_room`; a name
/// wider than `session_block_room` carries its own full width out.
fn build_session_block(session_name: &str, session_block_room: u16) -> SessionBlock {
    let session_name_text = format!(" {} ", session_name);
    let version_badge_text = format_version_badge_text();
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
/// cells before the tab strip's end column, as in
/// [`build_session_block`], and decides whether the badge is there.
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
fn get_tab_text_blocks(
    tabs_metadata: &[TabMetadata],
    tab_metadata_index: usize,
) -> (String, String) {
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
    tabs_metadata: &[TabMetadata],
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
