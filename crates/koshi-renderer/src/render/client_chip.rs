//! The client chips: a client number with one blank cell on each side, in
//! the client's colors, packed left to right with one cell between two chips,
//! and the `+N` mark that counts the clients with no chip. The tab strip
//! draws the viewing client and its peers, and a pane's top border or stack
//! header draws the peers whose keys reach that pane. A group draws at most
//! [`MAX_DRAWN_CLIENT_CHIP_COUNT`] chips.

use ratatui::buffer::Buffer;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use koshi_core::client::compute_client_number;

use crate::theme::ClientColor;

use super::{compute_client_color_style, set_line_clipped};

#[cfg(test)]
mod tests;

/// The most chips one group draws. Every further client of the group goes
/// into the `+N` mark.
pub(crate) const MAX_DRAWN_CLIENT_CHIP_COUNT: usize = 3;

/// One client chip: ` 3 `, the client number with one blank cell on each
/// side. The number is `color_index + 1` ([`compute_client_number`]), so
/// index `11` draws ` 12 ` in four cells.
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

/// The `+N` mark right of the client chips: `N` clients of the group have
/// no chip drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HiddenClientMark {
    /// The column of the `+`.
    pub start_column: u16,
    /// The count of the group's clients with no chip drawn, at least `1`.
    pub client_count: usize,
}

/// The chips and the `+N` mark of one group of clients, placed on a row
/// ([`solve_client_chip_group`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientChipGroup {
    /// The column of the group's first cell: the first chip's, or the mark's
    /// when no chip is drawn.
    pub start_column: u16,
    /// The chips drawn, left to right: the first clients of the group, in
    /// the group's order.
    pub client_chip_spans: Vec<ClientChipSpan>,
    /// The mark right of the chips. `None` while every client of the group
    /// has a chip.
    pub hidden_client_mark: Option<HiddenClientMark>,
}

/// The chips of `chip_clients`, each `(color_index, is_viewer_chip)`, and
/// the `+N` mark, packed into `group_room_width` cells and right-aligned so
/// that the last one ends at `group_end_column`. `client_count` counts the
/// clients of `chip_clients`. `group_room_width` is `group_end_column` or
/// less.
///
/// 1. At most [`MAX_DRAWN_CLIENT_CHIP_COUNT`] chips are drawn, the first
///    clients of `chip_clients` in order.
/// 2. When every chip fits the width and the cap, every chip is drawn and
///    no mark.
/// 3. Otherwise the mark `+N` follows the chips, one cell apart. `N` counts
///    the clients with no chip, and the mark takes `+` and the digits of
///    `N`. The chips drawn are the most that fit together with their mark.
///
/// `None` when `client_count` is `0`, or when the mark alone does not fit.
///
/// Example: in 12 cells that end at column `24`, the clients of indexes `0`
/// to `4` give ` 1 ` at `14`, ` 2 ` at `18` and `+3` at `22`: ` 1   2  +3`.
pub(super) fn solve_client_chip_group(
    chip_clients: impl Iterator<Item = (u32, bool)>,
    client_count: usize,
    group_room_width: u16,
    group_end_column: u16,
) -> Option<ClientChipGroup> {
    if client_count == 0 {
        return None;
    }
    let (mut packed_chips, mut chips_width) = pack_client_chips(
        chip_clients.take(MAX_DRAWN_CLIENT_CHIP_COUNT),
        group_room_width,
    );
    let mut hidden_client_mark_width = 0;
    while packed_chips.len() < client_count {
        hidden_client_mark_width =
            compute_hidden_client_mark_width(client_count - packed_chips.len());
        let separator_width = if packed_chips.is_empty() { 0 } else { 1 };
        if chips_width + separator_width + hidden_client_mark_width <= group_room_width {
            break;
        }
        packed_chips.pop()?;
        chips_width = match packed_chips.last() {
            Some((color_index, _, chip_offset)) => {
                chip_offset + compute_client_chip_width(*color_index)
            }
            None => 0,
        };
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
    Some(ClientChipGroup {
        start_column: group_start_column,
        client_chip_spans,
        hidden_client_mark,
    })
}

/// The chips of `chip_clients`, each `(color_index, is_viewer_chip)`, that
/// fit `room_width` cells in order, with one cell between two chips,
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
pub(super) fn compute_client_chip_width(color_index: u32) -> u16 {
    count_decimal_digits(compute_client_number(color_index)) + 2
}

/// The cells the mark `+N` takes for `hidden_client_count` clients: `+` and
/// the digits of the count. `1` and `9` give `2`, and `10` gives `3`.
pub(super) fn compute_hidden_client_mark_width(hidden_client_count: usize) -> u16 {
    1 + count_decimal_digits(hidden_client_count as u64)
}

/// The digits `decimal_number` takes in decimal: `0` and `9` give `1`, `10`
/// gives `2`, and `u64::MAX` gives `20`.
fn count_decimal_digits(decimal_number: u64) -> u16 {
    decimal_number
        .checked_ilog10()
        .map_or(1, |decimal_log| decimal_log as u16 + 1)
}

/// Draw the chips and the `+N` mark of `client_chip_group` on the row
/// `row_index` of `screen_buffer`: each chip in the colors at its position in
/// `client_colors` ([`build_client_chip_line`]), and the mark in
/// `hidden_client_mark_style`. The cells between them keep what the row
/// holds. Each chip and the mark are cut to the buffer.
///
/// Example: the chips ` 2 ` and ` 4 ` one cell apart on a top border read
/// ` 2 ─ 4 `.
pub(super) fn draw_client_chip_group(
    client_chip_group: &ClientChipGroup,
    client_colors: &[ClientColor],
    hidden_client_mark_style: Style,
    row_index: u16,
    screen_buffer: &mut Buffer,
) {
    for (client_chip_span, client_color) in client_chip_group
        .client_chip_spans
        .iter()
        .zip(client_colors)
    {
        set_line_clipped(
            screen_buffer,
            client_chip_span.start_column,
            row_index,
            &build_client_chip_line(*client_chip_span, *client_color),
            compute_client_chip_width(client_chip_span.color_index),
        );
    }
    if let Some(hidden_client_mark) = client_chip_group.hidden_client_mark {
        set_line_clipped(
            screen_buffer,
            hidden_client_mark.start_column,
            row_index,
            &Line::from(Span::styled(
                format!("+{}", hidden_client_mark.client_count),
                hidden_client_mark_style,
            )),
            compute_hidden_client_mark_width(hidden_client_mark.client_count),
        );
    }
}

/// The chip line of `client_chip_span` in `client_color`: the client number
/// with one blank cell on each side, its number bold and underlined when the
/// chip names the viewing client. Each chip cell drops every attribute of the
/// cell it is drawn over, so a chip on a bold border is not bold. Index `2`
/// gives ` 3 `.
fn build_client_chip_line(
    client_chip_span: ClientChipSpan,
    client_color: ClientColor,
) -> Line<'static> {
    build_client_chip_text(
        client_chip_span.color_index,
        client_chip_span.is_viewer_chip,
        client_color,
    )
}

/// The chip text of the client of index `color_index` in `client_color`: the
/// client number with one blank cell on each side, its number bold and
/// underlined when `is_viewer_chip`. Each chip cell drops every attribute of
/// the cell it is drawn over. Index `2` gives ` 3 `.
pub(super) fn build_client_chip_text(
    color_index: u32,
    is_viewer_chip: bool,
    client_color: ClientColor,
) -> Line<'static> {
    let chip_style = compute_client_color_style(client_color).remove_modifier(Modifier::all());
    let client_number_text = compute_client_number(color_index).to_string();
    let client_number_style = if is_viewer_chip {
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
