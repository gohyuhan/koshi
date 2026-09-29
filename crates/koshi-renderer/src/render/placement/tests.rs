//! Tests for full-pane placement previews.

use super::*;

#[test]
fn soften_pane_area_tints_cells_and_keeps_the_outline_and_glyphs() {
    let theme = Theme::default();
    let mut screen_buffer = Buffer::empty(RatatuiRect::new(0, 0, 8, 3));
    screen_buffer.set_string(
        2,
        1,
        "shell",
        Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::REVERSED),
    );

    let moving_pane_background_color = compute_placement_background_color(
        theme.bar_background_color,
        theme.focused_border_color,
        MOVING_PANE_TINT_PERCENTAGE,
    );
    soften_pane_area(
        RatatuiRect::new(1, 0, 6, 3),
        moving_pane_background_color,
        theme.focused_border_color,
        &theme,
        &mut screen_buffer,
    );

    assert_eq!(screen_buffer[(2, 1)].symbol(), "s");
    assert_eq!(screen_buffer[(2, 1)].fg, theme.unfocused_border_color);
    assert_eq!(screen_buffer[(2, 1)].bg, Color::Rgb(0, 42, 51));
    assert_eq!(screen_buffer[(2, 1)].modifier, Modifier::empty());
    assert_eq!(screen_buffer[(1, 0)].fg, theme.focused_border_color);
    assert_eq!(screen_buffer[(0, 1)].fg, Color::Reset);
}

#[test]
fn placement_background_colors_distinguish_the_moving_and_destination_panes() {
    let theme = Theme::default();

    assert_eq!(
        compute_placement_background_color(
            theme.bar_background_color,
            theme.focused_border_color,
            MOVING_PANE_TINT_PERCENTAGE,
        ),
        Color::Rgb(0, 42, 51)
    );
    assert_eq!(
        compute_placement_background_color(
            theme.bar_background_color,
            theme.hover_border_color,
            DESTINATION_PANE_TINT_PERCENTAGE,
        ),
        Color::Rgb(22, 12, 33)
    );
    assert_eq!(
        compute_placement_background_color(Color::Reset, theme.focused_border_color, 24),
        Color::Reset
    );
}

#[test]
fn uncovered_message_area_prefers_wide_side_over_short_top_strip() {
    assert_eq!(
        find_legible_uncovered_content_area(
            RatatuiRect::new(0, 0, 40, 6),
            &[RatatuiRect::new(10, 1, 15, 5)],
            12,
        ),
        Some(RatatuiRect::new(25, 1, 15, 5))
    );
}

#[test]
fn uncovered_message_area_uses_other_side_when_header_covers_one_side() {
    assert_eq!(
        find_legible_uncovered_content_area(
            RatatuiRect::new(0, 0, 30, 5),
            &[
                RatatuiRect::new(10, 0, 10, 5),
                RatatuiRect::new(20, 0, 10, 5),
            ],
            8,
        ),
        Some(RatatuiRect::new(0, 0, 10, 5))
    );
}

#[test]
fn draw_placement_message_uses_full_sentence_and_detail_when_the_pane_has_room() {
    let theme = Theme::default();
    let mut screen_buffer = Buffer::empty(RatatuiRect::new(0, 0, 40, 7));
    let placement_message = PanePlacementMessage {
        full_text: "Moving pane will land here".to_string(),
        compact_text: "Moving pane",
        detail_text: Some("Enter to place".to_string()),
    };

    draw_placement_message(
        RatatuiRect::new(0, 0, 40, 7),
        &placement_message,
        theme.accent_color,
        theme.bar_background_color,
        &theme,
        &mut screen_buffer,
    );

    assert_eq!(screen_buffer[(7, 3)].symbol(), "M");
    assert_eq!(screen_buffer[(13, 4)].symbol(), "E");
    assert_eq!(screen_buffer[(7, 3)].fg, theme.accent_color);
}

#[test]
fn draw_placement_message_uses_compact_sentence_in_a_narrow_pane() {
    let theme = Theme::default();
    let mut screen_buffer = Buffer::empty(RatatuiRect::new(0, 0, 14, 3));
    let placement_message = PanePlacementMessage {
        full_text: "Moving pane will insert above".to_string(),
        compact_text: "Moving pane",
        detail_text: None,
    };

    draw_placement_message(
        RatatuiRect::new(0, 0, 14, 3),
        &placement_message,
        theme.accent_color,
        theme.bar_background_color,
        &theme,
        &mut screen_buffer,
    );

    assert_eq!(screen_buffer[(1, 1)].symbol(), "M");
    assert_eq!(screen_buffer[(11, 1)].symbol(), "e");
}

#[test]
fn draw_placement_message_uses_an_ellipsis_when_only_one_cell_fits() {
    let theme = Theme::default();
    let mut screen_buffer = Buffer::empty(RatatuiRect::new(0, 0, 1, 3));
    let placement_message = PanePlacementMessage {
        full_text: "Moving pane will insert above".to_string(),
        compact_text: "Moving pane",
        detail_text: None,
    };

    draw_placement_message(
        RatatuiRect::new(0, 0, 1, 3),
        &placement_message,
        theme.accent_color,
        theme.bar_background_color,
        &theme,
        &mut screen_buffer,
    );

    assert_eq!(screen_buffer[(0, 1)].symbol(), "…");
}
