//! Tests for the config schema defaults and color parsing.

use super::*;

use koshi_core::geometry::Direction;
use koshi_core::key::ModFlags;

use crate::error::ColorParseError;
use crate::key::Leader;

#[test]
fn default_loads_with_expected_values() {
    let config = KoshiConfig::default();

    assert_eq!(config.version, SCHEMA_VERSION);

    assert_eq!(config.pane.min_cols, 2);
    assert_eq!(config.pane.min_rows, 1);

    assert_eq!(config.scrollback.max_lines, 10_000);
    assert_eq!(config.scrollback.max_bytes, 32 * 1024 * 1024);

    assert_eq!(config.keybindings.chord_timeout_ms, 500);
    assert_eq!(config.keybindings.which_key_delay_ms, 300);
    assert_eq!(config.keybindings.max_chord_depth, 4);
    assert_eq!(config.keybindings.leader, Leader::Mods(ModFlags::CTRL));
    assert!(config.keybindings.modes.is_empty());

    assert_eq!(config.layout.new_pane_direction, Direction::Right);
    assert_eq!(config.layout.default_layout, None);

    assert!(config.plugins.entries.is_empty());

    assert!(config.mouse.border_resize);
    assert!(!config.mouse.border_resize_in_lock);
    assert!(config.mouse.click_to_focus);
    assert_eq!(config.mouse.scroll_lines, 3);

    assert!(!config.copy.copy_on_select);
    assert!(config.copy.trim_trailing_whitespace);
    assert_eq!(config.copy.clipboard, ClipboardBackend::Osc52);

    assert_eq!(config.terminal.term, "xterm-256color");
    assert_eq!(config.terminal.colorterm, "truecolor");
    assert_eq!(config.terminal.default_shell, None);

    assert_eq!(config.theme.name, "default");
}

#[test]
fn default_palette_has_expected_roles() {
    let palette = ColorPalette::default();
    assert_eq!(palette.background, RgbColor::new(0x1e, 0x1e, 0x1e));
    assert_eq!(palette.foreground, RgbColor::new(0xd4, 0xd4, 0xd4));
    assert_eq!(palette.accent, RgbColor::new(0x00, 0xaf, 0xd7));
    assert_eq!(palette.border_focused, RgbColor::new(0x00, 0xaf, 0xd7));
    assert_eq!(palette.border_unfocused, RgbColor::new(0x58, 0x58, 0x58));
}

#[test]
fn from_hex_parses_leading_hash() {
    assert_eq!(
        RgbColor::from_hex("#00afd7"),
        Ok(RgbColor::new(0x00, 0xaf, 0xd7))
    );
}

#[test]
fn from_hex_parses_bare_and_uppercase() {
    assert_eq!(
        RgbColor::from_hex("00AFD7"),
        Ok(RgbColor::new(0x00, 0xaf, 0xd7))
    );
}

#[test]
fn from_hex_rejects_wrong_length() {
    assert_eq!(
        RgbColor::from_hex("#fff"),
        Err(ColorParseError::BadLength { got: 3 })
    );
}

#[test]
fn from_hex_rejects_non_hex_digit() {
    assert_eq!(
        RgbColor::from_hex("#gggggg"),
        Err(ColorParseError::BadDigit {
            value: "gggggg".to_string()
        })
    );
}

#[test]
fn from_str_delegates_to_from_hex() {
    assert_eq!(
        "#123456".parse::<RgbColor>(),
        Ok(RgbColor::new(0x12, 0x34, 0x56))
    );
}

#[test]
fn mode_name_roundtrips() {
    let mode = ModeName::new("resize");
    assert_eq!(mode.as_str(), "resize");
}
