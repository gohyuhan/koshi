//! Turning a viewer's configured palette into the colors it paints with.
//!
//! A config theme names each chrome role as `#RRGGBB` text; the renderer wants
//! truecolor values. [`resolve_theme`] is that one conversion, run once when a
//! viewer's config loads or reloads rather than once per frame.

use koshi_config::types::{RgbColor, ThemeConfig};
use koshi_renderer::theme::Theme;
use ratatui::style::Color;

#[cfg(test)]
mod tests;

/// Resolve a config theme into the renderer [`Theme`] a viewer paints with:
/// each palette role's `#RRGGBB` value becomes the matching truecolor field.
/// For example, a theme with `ramp_start "#ff0000"` yields a `Theme` whose
/// first tab ribbon paints red. Resolving the default config theme yields
/// exactly [`Theme::default`], so a default config reproduces the stock look.
#[must_use]
pub fn resolve_theme(theme_config: &ThemeConfig) -> Theme {
    let theme_colors = &theme_config.colors;
    Theme {
        ramp_start: extract_rgb_channels(theme_colors.ramp_start),
        ramp_end: extract_rgb_channels(theme_colors.ramp_end),
        ramp_block_text_color: convert_rgb_color(theme_colors.on_ramp),
        dimmed_ramp_text_color: convert_rgb_color(theme_colors.on_ramp_dim),
        accent_color: convert_rgb_color(theme_colors.accent),
        accent_block_text_color: convert_rgb_color(theme_colors.on_accent),
        focused_border_color: convert_rgb_color(theme_colors.border_focused),
        unfocused_border_color: convert_rgb_color(theme_colors.border_unfocused),
        hover_border_color: convert_rgb_color(theme_colors.border_hover),
        stack_header_text_color: convert_rgb_color(theme_colors.stack_header_fg),
        stack_header_background_color: convert_rgb_color(theme_colors.stack_header_bg),
        letterbox_color: convert_rgb_color(theme_colors.letterbox),
        bar_background_color: convert_rgb_color(theme_colors.bar_bg),
    }
}

/// A config color's `(r, g, b)` channels, for the theme's ramp endpoints.
fn extract_rgb_channels(rgb_color_value: RgbColor) -> (u8, u8, u8) {
    (
        rgb_color_value.red,
        rgb_color_value.green,
        rgb_color_value.blue,
    )
}

/// A config color as a ratatui truecolor.
fn convert_rgb_color(rgb_color_value: RgbColor) -> Color {
    Color::Rgb(
        rgb_color_value.red,
        rgb_color_value.green,
        rgb_color_value.blue,
    )
}
