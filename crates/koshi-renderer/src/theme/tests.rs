//! Tests for the chrome theme: default ramp endpoint stops, monotonic blend,
//! the single-element run, the dimmed variant, a custom-endpoint ramp, and the
//! stock palette the default theme carries.

use super::*;

use std::ops::Range;
use std::path::Path;

use koshi_config::theme::parse_theme;
use koshi_config::types::{ColorPalette, RgbColor};

#[test]
fn ramp_endpoints_are_the_palette_ends() {
    let theme = Theme::default();
    assert_eq!(theme.get_ramp_color(0, 5), Color::Rgb(0xd0, 0xa5, 0xff));
    assert_eq!(theme.get_ramp_color(4, 5), Color::Rgb(0x7d, 0xbc, 0xff));
}

#[test]
fn single_element_ramp_uses_the_start_color() {
    assert_eq!(
        Theme::default().get_ramp_color(0, 1),
        Color::Rgb(0xd0, 0xa5, 0xff)
    );
}

#[test]
fn ramp_color_clamps_an_out_of_range_index_to_the_last_color() {
    let theme = Theme::default();
    assert_eq!(theme.get_ramp_color(9, 3), Color::Rgb(0x7d, 0xbc, 0xff));
    assert_eq!(theme.get_ramp_color(2, 3), Color::Rgb(0x7d, 0xbc, 0xff));
}

#[test]
fn middle_stops_sit_between_the_ends() {
    let Color::Rgb(red_channel, green_channel, blue_channel) =
        Theme::default().get_ramp_color(1, 3)
    else {
        panic!("ramp yields Rgb");
    };
    assert_eq!(
        (red_channel, green_channel, blue_channel),
        (0xa7, 0xb0, 0xff)
    );
}

#[test]
fn dimmed_ramp_color_scales_every_rgb_channel() {
    let Color::Rgb(red_channel, green_channel, blue_channel) =
        Theme::default().get_dimmed_ramp_color(0, 1)
    else {
        panic!("dimmed ramp color yields RGB");
    };
    assert_eq!(
        (red_channel, green_channel, blue_channel),
        (0x72, 0x5a, 0x8c)
    );
}

#[test]
fn zero_element_ramp_returns_the_start_color_without_dividing_by_zero() {
    // A zero element count gives `compute_interpolated_channel` a zero final
    // index, so it returns the start channel without dividing by zero.
    let theme = Theme::default();
    assert_eq!(theme.get_ramp_color(0, 0), Color::Rgb(0xd0, 0xa5, 0xff));
    assert_eq!(theme.get_ramp_color(7, 0), Color::Rgb(0xd0, 0xa5, 0xff));
}

#[test]
fn dimmed_ramp_color_tracks_the_ramp_stop_it_scales() {
    // The dim of the far ramp end is that end pulled to 55% of each channel.
    let theme = Theme::default();
    assert_eq!(
        theme.get_dimmed_ramp_color(1, 2),
        Color::Rgb(0x44, 0x67, 0x8c)
    );
}

#[test]
fn every_stop_of_a_five_element_run_is_exact() {
    let theme = Theme::default();
    let ramp_colors: Vec<Color> = (0..5)
        .map(|ramp_stop_index| theme.get_ramp_color(ramp_stop_index, 5))
        .collect();
    assert_eq!(
        ramp_colors,
        vec![
            Color::Rgb(0xd0, 0xa5, 0xff),
            Color::Rgb(0xbc, 0xaa, 0xff),
            Color::Rgb(0xa7, 0xb0, 0xff),
            Color::Rgb(0x92, 0xb6, 0xff),
            Color::Rgb(0x7d, 0xbc, 0xff),
        ]
    );
}

#[test]
fn an_index_at_and_past_the_run_length_clamps_to_the_last_stop() {
    let theme = Theme::default();
    assert_eq!(theme.get_ramp_color(3, 3), Color::Rgb(0x7d, 0xbc, 0xff));
    assert_eq!(
        theme.get_ramp_color(usize::MAX, 3),
        Color::Rgb(0x7d, 0xbc, 0xff)
    );
}

#[test]
fn dimmed_ramp_color_uses_ramp_clamping_for_indices_and_element_counts() {
    let theme = Theme::default();
    // Past the last stop of a three-element run, and a zero-element run.
    assert_eq!(
        theme.get_dimmed_ramp_color(9, 3),
        Color::Rgb(0x44, 0x67, 0x8c)
    );
    assert_eq!(
        theme.get_dimmed_ramp_color(0, 0),
        Color::Rgb(0x72, 0x5a, 0x8c)
    );
}

#[test]
fn dimmed_ramp_color_keeps_black_and_scales_white_to_fifty_five_percent() {
    let theme = Theme {
        ramp_start: (0x00, 0x00, 0x00),
        ramp_end: (0xff, 0xff, 0xff),
        ..Theme::default()
    };
    assert_eq!(
        theme.get_dimmed_ramp_color(0, 2),
        Color::Rgb(0x00, 0x00, 0x00)
    );
    assert_eq!(
        theme.get_dimmed_ramp_color(1, 2),
        Color::Rgb(0x8c, 0x8c, 0x8c)
    );
}

#[test]
fn the_default_theme_is_the_config_crates_default_palette() {
    let theme = Theme::default();
    let palette = ColorPalette::default();
    let to_ratatui_rgb_color = |color: RgbColor| Color::Rgb(color.red, color.green, color.blue);
    let extract_rgb_channels = |color: RgbColor| (color.red, color.green, color.blue);

    assert_eq!(theme.ramp_start, extract_rgb_channels(palette.ramp_start));
    assert_eq!(theme.ramp_end, extract_rgb_channels(palette.ramp_end));
    assert_eq!(
        theme.ramp_block_text_color,
        to_ratatui_rgb_color(palette.on_ramp)
    );
    assert_eq!(
        theme.dimmed_ramp_text_color,
        to_ratatui_rgb_color(palette.on_ramp_dim)
    );
    assert_eq!(theme.accent_color, to_ratatui_rgb_color(palette.accent));
    assert_eq!(
        theme.accent_block_text_color,
        to_ratatui_rgb_color(palette.on_accent)
    );
    assert_eq!(
        theme.focused_border_color,
        to_ratatui_rgb_color(palette.border_focused)
    );
    assert_eq!(
        theme.unfocused_border_color,
        to_ratatui_rgb_color(palette.border_unfocused)
    );
    assert_eq!(
        theme.hover_border_color,
        to_ratatui_rgb_color(palette.border_hover)
    );
    assert_eq!(
        theme.stack_header_text_color,
        to_ratatui_rgb_color(palette.stack_header_fg)
    );
    assert_eq!(
        theme.stack_header_background_color,
        to_ratatui_rgb_color(palette.stack_header_bg)
    );
    assert_eq!(
        theme.letterbox_color,
        to_ratatui_rgb_color(palette.letterbox)
    );
    assert_eq!(
        Color::from(theme.bar_background_color),
        to_ratatui_rgb_color(palette.bar_bg)
    );
}

#[test]
fn every_default_color_outside_the_ramp_is_exact() {
    let theme = Theme::default();
    assert_eq!(theme.ramp_block_text_color, Color::Rgb(0x12, 0x09, 0x1f));
    assert_eq!(theme.dimmed_ramp_text_color, Color::Rgb(0xf0, 0xec, 0xfa));
    assert_eq!(theme.accent_color, Color::Rgb(0xf5, 0xc2, 0xff));
    assert_eq!(theme.accent_block_text_color, Color::Rgb(0x1e, 0x10, 0x33));
    assert_eq!(theme.focused_border_color, Color::Rgb(0x00, 0xaf, 0xd7));
    assert_eq!(theme.unfocused_border_color, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(theme.hover_border_color, Color::Rgb(0xaf, 0x5f, 0xff));
    assert_eq!(theme.stack_header_text_color, Color::Rgb(0xf4, 0xf1, 0xfa));
    assert_eq!(
        theme.stack_header_background_color,
        Color::Rgb(0x30, 0x0f, 0x4a)
    );
    assert_eq!(theme.letterbox_color, Color::Rgb(0x58, 0x58, 0x58));
    assert_eq!(theme.bar_background_color, (0x00, 0x00, 0x00));
}

#[test]
fn custom_endpoints_drive_the_ramp() {
    let theme = Theme {
        ramp_start: (0xff, 0x00, 0x00),
        ramp_end: (0x00, 0x00, 0xff),
        ..Theme::default()
    };
    assert_eq!(theme.get_ramp_color(0, 2), Color::Rgb(0xff, 0x00, 0x00));
    assert_eq!(theme.get_ramp_color(1, 2), Color::Rgb(0x00, 0x00, 0xff));
    // Midpoint by integer linear interpolation: red truncates toward zero
    // (255 - 255/2 = 128).
    assert_eq!(theme.get_ramp_color(1, 3), Color::Rgb(0x80, 0x00, 0x7f));
}

#[test]
fn ramp_interpolation_handles_the_maximum_element_count() {
    // The denominator is widened to i128, so it never wraps negative and flips
    // the interpolation past either end of the ramp.
    let theme = Theme::default();

    assert_eq!(
        theme.get_ramp_color(1, usize::MAX),
        theme.get_ramp_color(0, usize::MAX)
    );
}

// --- client colors -----------------------------------------------------------

const BLACK_TEXT_COLOR: Color = Color::Rgb(0x00, 0x00, 0x00);
const WHITE_TEXT_COLOR: Color = Color::Rgb(0xff, 0xff, 0xff);

/// The `(fill, text)` colors of each client index of `color_indexes` over
/// the bar of `theme`.
fn list_client_colors(theme: &Theme, color_indexes: Range<u32>) -> Vec<(Color, Color)> {
    color_indexes
        .map(|color_index| {
            let client_color = theme.compute_client_color(color_index);
            (
                Color::from(client_color.fill_color),
                Color::from(client_color.text_color),
            )
        })
        .collect()
}

#[test]
fn over_the_default_black_bar_the_first_twelve_clients_take_these_exact_colors() {
    assert_eq!(
        list_client_colors(&Theme::default(), 0..12),
        vec![
            (Color::Rgb(0xff, 0x00, 0x00), BLACK_TEXT_COLOR),
            (Color::Rgb(0x00, 0xff, 0x4a), BLACK_TEXT_COLOR),
            (Color::Rgb(0x95, 0x00, 0xff), WHITE_TEXT_COLOR),
            (Color::Rgb(0xff, 0xdf, 0x00), BLACK_TEXT_COLOR),
            (Color::Rgb(0x00, 0xd4, 0xff), BLACK_TEXT_COLOR),
            (Color::Rgb(0xff, 0x00, 0x8a), BLACK_TEXT_COLOR),
            (Color::Rgb(0x40, 0xff, 0x00), BLACK_TEXT_COLOR),
            (Color::Rgb(0x3c, 0x33, 0xff), WHITE_TEXT_COLOR),
            (Color::Rgb(0xff, 0x55, 0x00), BLACK_TEXT_COLOR),
            (Color::Rgb(0x00, 0xff, 0xa0), BLACK_TEXT_COLOR),
            (Color::Rgb(0xea, 0x00, 0xff), BLACK_TEXT_COLOR),
            (Color::Rgb(0xca, 0xff, 0x00), BLACK_TEXT_COLOR),
        ]
    );
}

#[test]
fn over_a_light_bar_a_fill_below_three_to_one_steps_darker_until_it_reaches_it() {
    let light_bar_theme = Theme {
        bar_background_color: (0xef, 0xf1, 0xf5),
        ..Theme::default()
    };

    assert_eq!(
        list_client_colors(&light_bar_theme, 0..6),
        vec![
            (Color::Rgb(0xff, 0x00, 0x00), BLACK_TEXT_COLOR),
            (Color::Rgb(0x00, 0x9e, 0x2e), BLACK_TEXT_COLOR),
            (Color::Rgb(0x95, 0x00, 0xff), WHITE_TEXT_COLOR),
            (Color::Rgb(0x9e, 0x8a, 0x00), BLACK_TEXT_COLOR),
            (Color::Rgb(0x00, 0x95, 0xb3), BLACK_TEXT_COLOR),
            (Color::Rgb(0xff, 0x00, 0x8a), BLACK_TEXT_COLOR),
        ]
    );
}

#[test]
fn convert_hsl_to_rgb_gives_the_css_color_at_full_saturation() {
    assert_eq!(convert_hsl_to_rgb(0.0, 50), (255, 0, 0));
    assert_eq!(convert_hsl_to_rgb(240.0, 60), (51, 51, 255));
    assert_eq!(convert_hsl_to_rgb(77.0, 100), (255, 255, 255));
    assert_eq!(convert_hsl_to_rgb(120.0, 0), (0, 0, 0));
}

#[test]
fn relative_luminance_and_contrast_ratio_give_the_wcag_values() {
    assert_eq!(compute_relative_luminance((255, 0, 0)), 0.2126);
    assert_eq!(compute_relative_luminance((0, 0, 0)), 0.0);
    assert_eq!(compute_contrast_ratio(1.0, 0.0), 21.0);
    assert_eq!(compute_contrast_ratio(0.0, 1.0), 21.0);
}

/// The bar background of the default palette and of every theme in
/// `themes-example/`, each with its file name.
fn list_shipped_bar_background_colors() -> Vec<(String, (u8, u8, u8))> {
    let default_bar_background_color = ColorPalette::default().bar_bg;
    let mut bar_background_colors = vec![(
        "the default palette".to_string(),
        (
            default_bar_background_color.red,
            default_bar_background_color.green,
            default_bar_background_color.blue,
        ),
    )];
    let themes_directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../themes-example");
    for theme_directory_entry in
        std::fs::read_dir(&themes_directory).expect("themes-example directory exists")
    {
        let theme_path = theme_directory_entry
            .expect("readable directory entry")
            .path();
        if theme_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("kdl")
        {
            continue;
        }
        let theme_file_name = theme_path
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        let theme_source_text =
            std::fs::read_to_string(&theme_path).expect("theme file is readable");
        let (parsed_theme, _) =
            parse_theme(&theme_path, &theme_source_text).unwrap_or_else(|parse_error| {
                panic!("{theme_file_name} does not parse: {parse_error}")
            });
        let bar_background_color = parsed_theme
            .colors
            .and_then(|colors| colors.bar_bg)
            .unwrap_or_else(|| panic!("{theme_file_name} does not set `bar-bg`"));
        bar_background_colors.push((
            theme_file_name,
            (
                bar_background_color.red,
                bar_background_color.green,
                bar_background_color.blue,
            ),
        ));
    }
    bar_background_colors
}

#[test]
fn over_every_shipped_bar_the_first_hundred_clients_reach_three_to_one_fill_and_four_and_a_half_to_one_text(
) {
    let bar_background_colors = list_shipped_bar_background_colors();
    assert_eq!(bar_background_colors.len(), 26);
    for (theme_name, bar_background_color) in bar_background_colors {
        let theme = Theme {
            bar_background_color,
            ..Theme::default()
        };
        let bar_luminance = compute_relative_luminance(bar_background_color);
        for color_index in 0..100 {
            let client_color = theme.compute_client_color(color_index);
            let fill_luminance = compute_relative_luminance(client_color.fill_color);
            let fill_contrast_ratio = compute_contrast_ratio(fill_luminance, bar_luminance);
            assert!(
                fill_contrast_ratio >= 3.0,
                "client {color_index} over {theme_name}: fill {:?} has {fill_contrast_ratio:.3}:1",
                client_color.fill_color
            );
            let text_contrast_ratio = compute_contrast_ratio(
                compute_relative_luminance(client_color.text_color),
                fill_luminance,
            );
            assert!(
                text_contrast_ratio >= 4.5,
                "client {color_index} over {theme_name}: text {:?} on fill {:?} has {text_contrast_ratio:.3}:1",
                client_color.text_color,
                client_color.fill_color
            );
        }
    }
}
