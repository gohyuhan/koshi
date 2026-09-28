//! Whole-pane placement previews in the viewer's pane area.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect as RatatuiRect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use koshi_core::geometry::Point;
use koshi_core::ids::PaneId;

use crate::snapshot::{PanePlacementMessage, PanePlacementPresentation, RenderSnapshot};
use crate::theme::Theme;

use super::{get_text_width, place_cell_rect, set_line_clipped};

const MOVING_PANE_TINT_PERCENTAGE: u16 = 24;
const DESTINATION_PANE_TINT_PERCENTAGE: u16 = 13;

/// Tint affected panes by role, draw one message per visible role, and give the
/// moving pane priority when pane rectangles overlap.
pub(super) fn draw_pane_placement_presentation(
    render_snapshot: &RenderSnapshot,
    placement_presentation: &PanePlacementPresentation,
    layout_origin: Point,
    theme: &Theme,
    screen_buffer: &mut Buffer,
) {
    let moving_pane_background_color = compute_placement_background_color(
        theme.bar_background_color,
        theme.focused_border_color,
        MOVING_PANE_TINT_PERCENTAGE,
    );
    let destination_pane_background_color = compute_placement_background_color(
        theme.bar_background_color,
        theme.hover_border_color,
        DESTINATION_PANE_TINT_PERCENTAGE,
    );
    let tab_snapshot = &render_snapshot.session_snapshot.active_tab_snapshot;
    let pane_areas = tab_snapshot
        .pane_slots
        .iter()
        .filter(|pane_slot| pane_slot.is_visible)
        .map(|pane_slot| (pane_slot.pane_id, pane_slot.outer_rect))
        .chain(
            tab_snapshot
                .stack_headers
                .iter()
                .map(|stack_header| (stack_header.pane_id, stack_header.header_rect)),
        );
    let mut source_pane_area = None;
    for (pane_id, pane_rect) in pane_areas {
        if pane_id == placement_presentation.source_pane_id {
            source_pane_area = Some(place_cell_rect(pane_rect, layout_origin));
        } else if placement_presentation.target_pane_ids.contains(&pane_id) {
            soften_pane_area(
                place_cell_rect(pane_rect, layout_origin),
                destination_pane_background_color,
                theme.hover_border_color,
                theme,
                screen_buffer,
            );
        }
    }
    if let Some(source_pane_area) = source_pane_area {
        soften_pane_area(
            source_pane_area,
            moving_pane_background_color,
            theme.focused_border_color,
            theme,
            screen_buffer,
        );
    }
    if let Some(target_message) = &placement_presentation.target_message {
        if let Some(target_pane_area) = find_legible_target_message_area(
            render_snapshot,
            &placement_presentation.target_pane_ids,
            get_text_width(target_message.compact_text).saturating_add(2),
            layout_origin,
            screen_buffer.area,
            source_pane_area,
        ) {
            draw_placement_message(
                target_pane_area,
                target_message,
                theme.hover_border_color,
                destination_pane_background_color,
                theme,
                screen_buffer,
            );
        }
    }
    if let Some(source_pane_area) = source_pane_area {
        draw_placement_message(
            source_pane_area,
            &placement_presentation.source_message,
            theme.focused_border_color,
            moving_pane_background_color,
            theme,
            screen_buffer,
        );
    }
}

fn compute_placement_background_color(
    background_color: Color,
    tint_color: Color,
    tint_percentage: u16,
) -> Color {
    let (
        Color::Rgb(background_red, background_green, background_blue),
        Color::Rgb(tint_red, tint_green, tint_blue),
    ) = (background_color, tint_color)
    else {
        return background_color;
    };
    let tint_percentage = tint_percentage.min(100);
    let blend_channel = |background_channel: u8, tint_channel: u8| -> u8 {
        ((u16::from(background_channel) * (100 - tint_percentage)
            + u16::from(tint_channel) * tint_percentage)
            / 100) as u8
    };
    Color::Rgb(
        blend_channel(background_red, tint_red),
        blend_channel(background_green, tint_green),
        blend_channel(background_blue, tint_blue),
    )
}

fn soften_pane_area(
    pane_area: RatatuiRect,
    background_color: Color,
    outline_color: Color,
    theme: &Theme,
    screen_buffer: &mut Buffer,
) {
    let visible_pane_area = pane_area.intersection(screen_buffer.area);
    let right_border_column = pane_area.right().saturating_sub(1);
    let bottom_border_row = pane_area.bottom().saturating_sub(1);
    for row_index in visible_pane_area.top()..visible_pane_area.bottom() {
        for column_index in visible_pane_area.left()..visible_pane_area.right() {
            let pane_cell = &mut screen_buffer[(column_index, row_index)];
            let is_outline_cell = column_index == pane_area.left()
                || column_index == right_border_column
                || row_index == pane_area.top()
                || row_index == bottom_border_row;
            pane_cell.set_fg(if is_outline_cell {
                outline_color
            } else {
                theme.unfocused_border_color
            });
            pane_cell.set_bg(background_color);
            pane_cell.modifier = Modifier::empty();
        }
    }
}

/// Return a visible target area that can show its short message when available.
/// Among equally legible areas, prefer larger areas, then wider areas.
fn find_legible_target_message_area(
    render_snapshot: &RenderSnapshot,
    target_pane_ids: &[PaneId],
    minimum_compact_message_column_count: u16,
    layout_origin: Point,
    visible_buffer_area: RatatuiRect,
    source_pane_area: Option<RatatuiRect>,
) -> Option<RatatuiRect> {
    let tab_snapshot = &render_snapshot.session_snapshot.active_tab_snapshot;
    let mut selected_pane_area: Option<RatatuiRect> = None;
    for pane_id in target_pane_ids {
        let pane_rect = tab_snapshot
            .pane_slots
            .iter()
            .find(|pane_slot| pane_slot.pane_id == *pane_id && pane_slot.is_visible)
            .map(|pane_slot| pane_slot.outer_rect)
            .or_else(|| {
                tab_snapshot
                    .stack_headers
                    .iter()
                    .find(|stack_header| stack_header.pane_id == *pane_id)
                    .map(|stack_header| stack_header.header_rect)
            });
        let Some(pane_rect) = pane_rect else {
            continue;
        };
        let pane_area = place_cell_rect(pane_rect, layout_origin).intersection(visible_buffer_area);
        let uncovered_pane_area = source_pane_area.map_or(Some(pane_area), |source_pane_area| {
            find_legible_uncovered_pane_area(
                pane_area,
                source_pane_area,
                minimum_compact_message_column_count,
            )
        });
        let Some(uncovered_pane_area) = uncovered_pane_area else {
            continue;
        };
        if uncovered_pane_area.width == 0 || uncovered_pane_area.height == 0 {
            continue;
        }
        if selected_pane_area.is_none_or(|current_selected_pane_area| {
            compute_message_area_priority(uncovered_pane_area, minimum_compact_message_column_count)
                > compute_message_area_priority(
                    current_selected_pane_area,
                    minimum_compact_message_column_count,
                )
        }) {
            selected_pane_area = Some(uncovered_pane_area);
        }
    }
    selected_pane_area
}

/// Return the most legible part of one pane that the moving pane does not cover.
fn find_legible_uncovered_pane_area(
    pane_area: RatatuiRect,
    source_pane_area: RatatuiRect,
    minimum_compact_message_column_count: u16,
) -> Option<RatatuiRect> {
    let overlap_area = pane_area.intersection(source_pane_area);
    if overlap_area.width == 0 || overlap_area.height == 0 {
        return Some(pane_area);
    }
    [
        RatatuiRect::new(
            pane_area.x,
            pane_area.y,
            pane_area.width,
            overlap_area.y - pane_area.y,
        ),
        RatatuiRect::new(
            pane_area.x,
            overlap_area.bottom(),
            pane_area.width,
            pane_area.bottom() - overlap_area.bottom(),
        ),
        RatatuiRect::new(
            pane_area.x,
            overlap_area.y,
            overlap_area.x - pane_area.x,
            overlap_area.height,
        ),
        RatatuiRect::new(
            overlap_area.right(),
            overlap_area.y,
            pane_area.right() - overlap_area.right(),
            overlap_area.height,
        ),
    ]
    .into_iter()
    .filter(|uncovered_area| uncovered_area.width > 0 && uncovered_area.height > 0)
    .max_by_key(|uncovered_area| {
        compute_message_area_priority(*uncovered_area, minimum_compact_message_column_count)
    })
}

fn compute_message_area_priority(
    uncovered_pane_area: RatatuiRect,
    minimum_compact_message_column_count: u16,
) -> (bool, u32, u16) {
    (
        uncovered_pane_area.width >= minimum_compact_message_column_count,
        uncovered_pane_area.area(),
        uncovered_pane_area.width,
    )
}

fn draw_placement_message(
    pane_area: RatatuiRect,
    placement_message: &PanePlacementMessage,
    text_color: Color,
    background_color: Color,
    theme: &Theme,
    screen_buffer: &mut Buffer,
) {
    let pane_area = pane_area.intersection(screen_buffer.area);
    let available_column_count = pane_area.width.saturating_sub(2);
    if available_column_count == 0 || pane_area.height == 0 {
        return;
    }
    let heading_text = if get_text_width(&placement_message.full_text) <= available_column_count {
        placement_message.full_text.as_str()
    } else if get_text_width(placement_message.compact_text) <= available_column_count {
        placement_message.compact_text
    } else {
        "…"
    };
    let visible_detail_text = placement_message
        .detail_text
        .as_deref()
        .filter(|detail_text| {
            pane_area.height >= 4 && get_text_width(detail_text) <= available_column_count
        });
    let heading_row =
        pane_area.y + (pane_area.height - u16::from(visible_detail_text.is_some())) / 2;
    let heading_column_count = get_text_width(heading_text).min(available_column_count);
    let heading_column = pane_area.x + (pane_area.width - heading_column_count) / 2;
    let heading_line = Line::from(Span::styled(
        heading_text,
        Style::default()
            .fg(text_color)
            .bg(background_color)
            .add_modifier(Modifier::BOLD),
    ));
    set_line_clipped(
        screen_buffer,
        heading_column,
        heading_row,
        &heading_line,
        heading_column_count,
    );
    if let Some(detail_text) = visible_detail_text {
        let detail_column_count = get_text_width(detail_text);
        let detail_column = pane_area.x + (pane_area.width - detail_column_count) / 2;
        let detail_line = Line::from(Span::styled(
            detail_text,
            Style::default()
                .fg(theme.dimmed_ramp_text_color)
                .bg(background_color),
        ));
        set_line_clipped(
            screen_buffer,
            detail_column,
            heading_row + 1,
            &detail_line,
            detail_column_count,
        );
    }
}

#[cfg(test)]
mod tests;
