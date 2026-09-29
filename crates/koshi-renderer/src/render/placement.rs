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

/// Tint affected panes by role, draw messages inside visible pane content
/// outside stack headers, and give the moving pane priority when rectangles
/// overlap.
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
    let source_pane_slot = tab_snapshot.pane_slots.iter().find(|pane_slot| {
        pane_slot.pane_id == placement_presentation.source_pane_id && pane_slot.is_visible
    });
    let source_visible_pane_area =
        source_pane_slot.map(|pane_slot| place_cell_rect(pane_slot.outer_rect, layout_origin));
    let has_source_stack_header = tab_snapshot
        .stack_headers
        .iter()
        .any(|stack_header| stack_header.pane_id == placement_presentation.source_pane_id);
    let mut blocked_message_areas = tab_snapshot
        .stack_headers
        .iter()
        .map(|stack_header| place_cell_rect(stack_header.header_rect, layout_origin))
        .collect::<Vec<_>>();
    let source_message_area = if has_source_stack_header {
        None
    } else {
        source_pane_slot
            .and_then(|pane_slot| pane_slot.content_rect)
            .map(|content_rect| place_cell_rect(content_rect, layout_origin))
            .and_then(|content_area| {
                find_legible_uncovered_content_area(
                    content_area.intersection(screen_buffer.area),
                    &blocked_message_areas,
                    get_text_width(placement_presentation.source_message.compact_text),
                )
            })
    };
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
    for (pane_id, pane_display_rect) in pane_areas.clone() {
        if pane_id != placement_presentation.source_pane_id
            && placement_presentation.target_pane_ids.contains(&pane_id)
        {
            soften_pane_area(
                place_cell_rect(pane_display_rect, layout_origin),
                destination_pane_background_color,
                theme.hover_border_color,
                theme,
                screen_buffer,
            );
        }
    }
    for (pane_id, pane_display_rect) in pane_areas {
        if pane_id == placement_presentation.source_pane_id {
            soften_pane_area(
                place_cell_rect(pane_display_rect, layout_origin),
                moving_pane_background_color,
                theme.focused_border_color,
                theme,
                screen_buffer,
            );
        }
    }
    if let Some(target_message) = &placement_presentation.target_message {
        if let Some(source_visible_pane_area) = source_visible_pane_area {
            blocked_message_areas.push(source_visible_pane_area);
        }
        if let Some(target_message_area) = find_legible_target_message_area(
            render_snapshot,
            &placement_presentation.target_pane_ids,
            get_text_width(target_message.compact_text),
            layout_origin,
            screen_buffer.area,
            &blocked_message_areas,
        ) {
            draw_placement_message(
                target_message_area,
                target_message,
                theme.hover_border_color,
                destination_pane_background_color,
                theme,
                screen_buffer,
            );
        }
    }
    if let Some(source_message_area) = source_message_area {
        draw_placement_message(
            source_message_area,
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
    blocked_message_areas: &[RatatuiRect],
) -> Option<RatatuiRect> {
    let tab_snapshot = &render_snapshot.session_snapshot.active_tab_snapshot;
    let mut selected_message_area: Option<RatatuiRect> = None;
    for pane_id in target_pane_ids {
        if tab_snapshot
            .stack_headers
            .iter()
            .any(|stack_header| stack_header.pane_id == *pane_id)
        {
            continue;
        }
        let pane_content_rect = tab_snapshot
            .pane_slots
            .iter()
            .find(|pane_slot| pane_slot.pane_id == *pane_id && pane_slot.is_visible)
            .and_then(|pane_slot| pane_slot.content_rect);
        let Some(pane_content_rect) = pane_content_rect else {
            continue;
        };
        let target_content_area =
            place_cell_rect(pane_content_rect, layout_origin).intersection(visible_buffer_area);
        let uncovered_content_area = find_legible_uncovered_content_area(
            target_content_area,
            blocked_message_areas,
            minimum_compact_message_column_count,
        );
        let Some(uncovered_content_area) = uncovered_content_area else {
            continue;
        };
        if uncovered_content_area.width == 0 || uncovered_content_area.height == 0 {
            continue;
        }
        if selected_message_area.is_none_or(|current_message_area| {
            compute_message_area_priority(
                uncovered_content_area,
                minimum_compact_message_column_count,
            ) > compute_message_area_priority(
                current_message_area,
                minimum_compact_message_column_count,
            )
        }) {
            selected_message_area = Some(uncovered_content_area);
        }
    }
    selected_message_area
}

/// Return the most legible content area outside the blocked message areas.
fn find_legible_uncovered_content_area(
    target_content_area: RatatuiRect,
    blocked_message_areas: &[RatatuiRect],
    minimum_compact_message_column_count: u16,
) -> Option<RatatuiRect> {
    let mut unexamined_blocked_message_areas = blocked_message_areas;
    let (overlap_area, blocked_message_areas_after_overlap) = loop {
        let Some((blocked_message_area, remaining_blocked_message_areas)) =
            unexamined_blocked_message_areas.split_first()
        else {
            return Some(target_content_area);
        };
        let overlap_area = target_content_area.intersection(*blocked_message_area);
        if overlap_area.width > 0 && overlap_area.height > 0 {
            break (overlap_area, remaining_blocked_message_areas);
        }
        unexamined_blocked_message_areas = remaining_blocked_message_areas;
    };
    [
        RatatuiRect::new(
            target_content_area.x,
            target_content_area.y,
            target_content_area.width,
            overlap_area.y - target_content_area.y,
        ),
        RatatuiRect::new(
            target_content_area.x,
            overlap_area.bottom(),
            target_content_area.width,
            target_content_area.bottom() - overlap_area.bottom(),
        ),
        RatatuiRect::new(
            target_content_area.x,
            overlap_area.y,
            overlap_area.x - target_content_area.x,
            overlap_area.height,
        ),
        RatatuiRect::new(
            overlap_area.right(),
            overlap_area.y,
            target_content_area.right() - overlap_area.right(),
            overlap_area.height,
        ),
    ]
    .into_iter()
    .filter(|uncovered_area| uncovered_area.width > 0 && uncovered_area.height > 0)
    .filter_map(|uncovered_area| {
        find_legible_uncovered_content_area(
            uncovered_area,
            blocked_message_areas_after_overlap,
            minimum_compact_message_column_count,
        )
    })
    .max_by_key(|uncovered_area| {
        compute_message_area_priority(*uncovered_area, minimum_compact_message_column_count)
    })
}

fn compute_message_area_priority(
    uncovered_content_area: RatatuiRect,
    minimum_compact_message_column_count: u16,
) -> (bool, u32, u16) {
    (
        uncovered_content_area.width >= minimum_compact_message_column_count,
        uncovered_content_area.area(),
        uncovered_content_area.width,
    )
}

fn draw_placement_message(
    message_area: RatatuiRect,
    placement_message: &PanePlacementMessage,
    text_color: Color,
    background_color: Color,
    theme: &Theme,
    screen_buffer: &mut Buffer,
) {
    let message_area = message_area.intersection(screen_buffer.area);
    let available_column_count = message_area.width;
    if available_column_count == 0 || message_area.height == 0 {
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
            message_area.height >= 2 && get_text_width(detail_text) <= available_column_count
        });
    let heading_row =
        message_area.y + (message_area.height - u16::from(visible_detail_text.is_some())) / 2;
    let heading_column_count = get_text_width(heading_text).min(available_column_count);
    let heading_column = message_area.x + (message_area.width - heading_column_count) / 2;
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
        let detail_column = message_area.x + (message_area.width - detail_column_count) / 2;
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
