//! Tests for resolving a viewer's configured palette into the colors it
//! paints with.

use super::*;

use koshi_config::types::ThemeConfig;

/// Resolving the default config theme yields exactly the renderer's default
/// theme: the two crates' stock palettes never drift apart.
#[test]
fn resolving_the_default_config_theme_is_the_default_theme() {
    assert_eq!(resolve_theme(&ThemeConfig::default()), Theme::default());
}

/// Each palette role lands on its matching theme field as a truecolor.
#[test]
fn resolve_theme_maps_every_palette_role() {
    let mut theme_config = ThemeConfig::default();
    theme_config.colors.ramp_start = RgbColor::from_channels(0x01, 0x02, 0x03);
    theme_config.colors.ramp_end = RgbColor::from_channels(0x04, 0x05, 0x06);
    theme_config.colors.on_ramp = RgbColor::from_channels(0x07, 0x08, 0x09);
    theme_config.colors.on_ramp_dim = RgbColor::from_channels(0x0a, 0x0b, 0x0c);
    theme_config.colors.accent = RgbColor::from_channels(0x0d, 0x0e, 0x0f);
    theme_config.colors.on_accent = RgbColor::from_channels(0x10, 0x11, 0x12);
    theme_config.colors.border_focused = RgbColor::from_channels(0x13, 0x14, 0x15);
    theme_config.colors.border_unfocused = RgbColor::from_channels(0x16, 0x17, 0x18);
    theme_config.colors.border_hover = RgbColor::from_channels(0x22, 0x23, 0x24);
    theme_config.colors.stack_header_fg = RgbColor::from_channels(0x19, 0x1a, 0x1b);
    theme_config.colors.stack_header_bg = RgbColor::from_channels(0x1c, 0x1d, 0x1e);
    theme_config.colors.letterbox = RgbColor::from_channels(0x1f, 0x20, 0x21);
    theme_config.colors.bar_bg = RgbColor::from_channels(0x25, 0x26, 0x27);

    let theme = resolve_theme(&theme_config);
    assert_eq!(theme.ramp_start, (0x01, 0x02, 0x03));
    assert_eq!(theme.ramp_end, (0x04, 0x05, 0x06));
    assert_eq!(theme.ramp_block_text_color, Color::Rgb(0x07, 0x08, 0x09));
    assert_eq!(theme.dimmed_ramp_text_color, Color::Rgb(0x0a, 0x0b, 0x0c));
    assert_eq!(theme.accent_color, Color::Rgb(0x0d, 0x0e, 0x0f));
    assert_eq!(theme.accent_block_text_color, Color::Rgb(0x10, 0x11, 0x12));
    assert_eq!(theme.focused_border_color, Color::Rgb(0x13, 0x14, 0x15));
    assert_eq!(theme.unfocused_border_color, Color::Rgb(0x16, 0x17, 0x18));
    assert_eq!(theme.hover_border_color, Color::Rgb(0x22, 0x23, 0x24));
    assert_eq!(theme.stack_header_text_color, Color::Rgb(0x19, 0x1a, 0x1b));
    assert_eq!(
        theme.stack_header_background_color,
        Color::Rgb(0x1c, 0x1d, 0x1e)
    );
    assert_eq!(theme.letterbox_color, Color::Rgb(0x1f, 0x20, 0x21));
    assert_eq!(theme.bar_background_color, Color::Rgb(0x25, 0x26, 0x27));
}
