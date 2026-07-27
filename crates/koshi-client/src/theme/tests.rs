//! Tests for resolving a viewer's configured palette into the colors it
//! paints with.

use super::*;

use koshi_config::types::ThemeConfig;

/// Resolving the default config theme yields exactly the renderer's default
/// theme: the two crates' stock palettes never drift apart.
#[test]
fn resolving_the_default_config_theme_is_the_default_theme() {
    assert_eq!(resolve(&ThemeConfig::default()), Theme::default());
}

/// Each palette role lands on its matching theme field as a truecolor.
#[test]
fn resolve_maps_every_palette_role() {
    let mut config = ThemeConfig::default();
    config.colors.ramp_start = RgbColor::new(0x01, 0x02, 0x03);
    config.colors.ramp_end = RgbColor::new(0x04, 0x05, 0x06);
    config.colors.on_ramp = RgbColor::new(0x07, 0x08, 0x09);
    config.colors.on_ramp_dim = RgbColor::new(0x0a, 0x0b, 0x0c);
    config.colors.accent = RgbColor::new(0x0d, 0x0e, 0x0f);
    config.colors.on_accent = RgbColor::new(0x10, 0x11, 0x12);
    config.colors.border_focused = RgbColor::new(0x13, 0x14, 0x15);
    config.colors.border_unfocused = RgbColor::new(0x16, 0x17, 0x18);
    config.colors.border_hover = RgbColor::new(0x22, 0x23, 0x24);
    config.colors.stack_header_fg = RgbColor::new(0x19, 0x1a, 0x1b);
    config.colors.stack_header_bg = RgbColor::new(0x1c, 0x1d, 0x1e);
    config.colors.letterbox = RgbColor::new(0x1f, 0x20, 0x21);
    config.colors.bar_bg = RgbColor::new(0x25, 0x26, 0x27);

    let theme = resolve(&config);
    assert_eq!(theme.ramp_start, (0x01, 0x02, 0x03));
    assert_eq!(theme.ramp_end, (0x04, 0x05, 0x06));
    assert_eq!(theme.on_ramp, Color::Rgb(0x07, 0x08, 0x09));
    assert_eq!(theme.on_ramp_dim, Color::Rgb(0x0a, 0x0b, 0x0c));
    assert_eq!(theme.accent, Color::Rgb(0x0d, 0x0e, 0x0f));
    assert_eq!(theme.on_accent, Color::Rgb(0x10, 0x11, 0x12));
    assert_eq!(theme.border_focused, Color::Rgb(0x13, 0x14, 0x15));
    assert_eq!(theme.border_unfocused, Color::Rgb(0x16, 0x17, 0x18));
    assert_eq!(theme.border_hover, Color::Rgb(0x22, 0x23, 0x24));
    assert_eq!(theme.stack_header_fg, Color::Rgb(0x19, 0x1a, 0x1b));
    assert_eq!(theme.stack_header_bg, Color::Rgb(0x1c, 0x1d, 0x1e));
    assert_eq!(theme.letterbox, Color::Rgb(0x1f, 0x20, 0x21));
    assert_eq!(theme.bar_bg, Color::Rgb(0x25, 0x26, 0x27));
}
