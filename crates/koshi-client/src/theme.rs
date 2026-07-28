//! Turning a viewer's configured palette into the colors it paints with.
//!
//! A config theme names each chrome role as `#RRGGBB` text; the renderer wants
//! truecolor values. [`resolve`] is that one conversion, run once when a
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
pub fn resolve(config: &ThemeConfig) -> Theme {
    let colors = &config.colors;
    Theme {
        ramp_start: rgb_channels(colors.ramp_start),
        ramp_end: rgb_channels(colors.ramp_end),
        on_ramp: rgb_color(colors.on_ramp),
        on_ramp_dim: rgb_color(colors.on_ramp_dim),
        accent: rgb_color(colors.accent),
        on_accent: rgb_color(colors.on_accent),
        border_focused: rgb_color(colors.border_focused),
        border_unfocused: rgb_color(colors.border_unfocused),
        border_hover: rgb_color(colors.border_hover),
        stack_header_fg: rgb_color(colors.stack_header_fg),
        stack_header_bg: rgb_color(colors.stack_header_bg),
        letterbox: rgb_color(colors.letterbox),
        bar_bg: rgb_color(colors.bar_bg),
    }
}

/// A config color's `(r, g, b)` channels, for the theme's ramp endpoints.
fn rgb_channels(color: RgbColor) -> (u8, u8, u8) {
    (color.r, color.g, color.b)
}

/// A config color as a ratatui truecolor.
fn rgb_color(color: RgbColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}
