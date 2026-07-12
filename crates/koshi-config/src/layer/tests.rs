//! Tests for config layering: precedence, deep field-level merge, and the
//! whole-value replace of collection fields.

use std::collections::BTreeMap;

use koshi_core::geometry::Direction;
use koshi_core::key::{Key, KeyChord, ModFlags};

use super::*;
use crate::types::{
    ActivationAction, ActivationScope, ClipboardBackend, KeymapOptIn, KoshiConfig, ModeBindings,
    ModeName, RgbColor,
};

#[test]
fn empty_layer_changes_nothing() {
    let merged = merge(KoshiConfig::default(), vec![PartialKoshiConfig::default()]);
    assert_eq!(merged, KoshiConfig::default());
}

#[test]
fn no_layers_returns_base() {
    let merged = merge(KoshiConfig::default(), vec![]);
    assert_eq!(merged, KoshiConfig::default());
}

#[test]
fn single_field_override_keeps_sibling() {
    let layer = PartialKoshiConfig {
        scrollback: Some(PartialScrollbackConfig {
            max_lines: Some(5_000),
            max_bytes: None,
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![layer]);

    assert_eq!(merged.scrollback.max_lines, 5_000);
    // Sibling untouched: keeps the default.
    assert_eq!(merged.scrollback.max_bytes, 32 * 1024 * 1024);
}

#[test]
fn later_layer_wins_on_same_field() {
    let user = PartialKoshiConfig {
        scrollback: Some(PartialScrollbackConfig {
            max_lines: Some(5_000),
            max_bytes: None,
        }),
        ..Default::default()
    };
    let session = PartialKoshiConfig {
        scrollback: Some(PartialScrollbackConfig {
            max_lines: Some(20_000),
            max_bytes: None,
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![user, session]);

    assert_eq!(merged.scrollback.max_lines, 20_000);
    assert_eq!(merged.scrollback.max_bytes, 32 * 1024 * 1024);
}

#[test]
fn sections_from_different_layers_combine() {
    let user = PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            min_cols: Some(10),
            min_rows: None,
        }),
        ..Default::default()
    };
    let session = PartialKoshiConfig {
        mouse: Some(PartialMouseConfig {
            scroll_lines: Some(7),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![user, session]);

    assert_eq!(merged.pane.min_cols, 10);
    assert_eq!(merged.pane.min_rows, 1);
    assert_eq!(merged.mouse.scroll_lines, 7);
    assert!(merged.mouse.border_resize);
}

#[test]
fn copy_and_terminal_scalar_overrides() {
    let layer = PartialKoshiConfig {
        copy: Some(PartialCopyConfig {
            copy_on_select: Some(true),
            trim_trailing_whitespace: None,
            clipboard: Some(ClipboardBackend::Native),
        }),
        terminal: Some(PartialTerminalConfig {
            term: Some("screen-256color".to_string()),
            colorterm: None,
            default_shell: None,
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![layer]);

    assert!(merged.copy.copy_on_select);
    assert!(merged.copy.trim_trailing_whitespace); // default kept
    assert_eq!(merged.copy.clipboard, ClipboardBackend::Native);
    assert_eq!(merged.terminal.term, "screen-256color");
    assert_eq!(merged.terminal.colorterm, "truecolor"); // default kept
}

#[test]
fn terminal_default_shell_sets_inner_value() {
    let layer = PartialKoshiConfig {
        terminal: Some(PartialTerminalConfig {
            term: None,
            colorterm: None,
            default_shell: Some(Some("/bin/zsh".to_string())),
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![layer]);
    assert_eq!(merged.terminal.default_shell, Some("/bin/zsh".to_string()));
}

#[test]
fn layout_direction_and_default_layout_override() {
    let layer = PartialKoshiConfig {
        layout: Some(PartialLayoutDefaults {
            new_pane_direction: Some(Direction::Down),
            default_layout: Some(Some("dev".to_string())),
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![layer]);

    assert_eq!(merged.layout.new_pane_direction, Direction::Down);
    assert_eq!(merged.layout.default_layout, Some("dev".to_string()));
}

#[test]
fn deep_theme_color_override_keeps_other_roles() {
    let overridden = RgbColor::new(0xff, 0x00, 0x00);
    let layer = PartialKoshiConfig {
        theme: Some(PartialThemeConfig {
            name: None,
            colors: Some(PartialColorPalette {
                accent: Some(overridden),
                ..Default::default()
            }),
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![layer]);
    let default_palette = KoshiConfig::default().theme.colors;

    assert_eq!(merged.theme.colors.accent, overridden);
    // Every other role keeps its default.
    assert_eq!(merged.theme.colors.ramp_start, default_palette.ramp_start);
    assert_eq!(merged.theme.colors.ramp_end, default_palette.ramp_end);
    assert_eq!(merged.theme.name, "default"); // sibling field kept
}

#[test]
fn logging_override_enables_the_log_file() {
    let layer = PartialKoshiConfig {
        logging: Some(PartialLoggingConfig {
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![layer]);

    assert!(merged.logging.enabled);
    // An absent logging section leaves the default (disabled) in place.
    let untouched = merge(KoshiConfig::default(), vec![PartialKoshiConfig::default()]);
    assert!(!untouched.logging.enabled);
}

#[test]
fn modes_replaced_wholesale() {
    let mut base = KoshiConfig::default();
    base.keybindings
        .modes
        .insert(ModeName::new("normal"), ModeBindings::default());

    let mut override_map = BTreeMap::new();
    override_map.insert(ModeName::new("resize"), ModeBindings::default());
    let layer = PartialKoshiConfig {
        keybindings: Some(PartialKeybindingsConfig {
            modes: Some(override_map),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged = merge(base, vec![layer]);

    // The whole map is replaced: the base's "normal" entry is gone.
    assert_eq!(merged.keybindings.modes.len(), 1);
    assert!(merged
        .keybindings
        .modes
        .contains_key(&ModeName::new("resize")));
    assert!(!merged
        .keybindings
        .modes
        .contains_key(&ModeName::new("normal")));
}

#[test]
fn unlock_alternative_layers_as_a_nested_option() {
    let alternative = KeyChord::new(ModFlags::CTRL, Key::Char('u'));
    let set = PartialKoshiConfig {
        keybindings: Some(PartialKeybindingsConfig {
            unlock_alternative: Some(Some(alternative)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![set]);
    assert_eq!(merged.keybindings.unlock_alternative, Some(alternative));

    // A later layer can set the value back to "keep the built-in unlock key".
    let clear = PartialKoshiConfig {
        keybindings: Some(PartialKeybindingsConfig {
            unlock_alternative: Some(None),
            ..Default::default()
        }),
        ..Default::default()
    };
    let cleared = merge(merged, vec![clear]);
    assert_eq!(cleared.keybindings.unlock_alternative, None);

    // A layer that leaves the field unset keeps the lower layer's value.
    assert_eq!(
        merge(KoshiConfig::default(), vec![PartialKoshiConfig::default()])
            .keybindings
            .unlock_alternative,
        None
    );
}

#[test]
fn keybindings_scalars_keep_untouched_siblings() {
    let layer = PartialKoshiConfig {
        keybindings: Some(PartialKeybindingsConfig {
            leader: Some(Leader::Mods(ModFlags::ALT)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![layer]);

    assert_eq!(merged.keybindings.leader, Leader::Mods(ModFlags::ALT));
    assert_eq!(merged.keybindings.chord_timeout_ms, 500); // default kept
    assert_eq!(merged.keybindings.max_chord_depth, 4); // default kept
}

#[test]
fn plugin_entries_replaced_wholesale() {
    let entry = PluginActivation {
        name: "statusbar".to_string(),
        action: ActivationAction::Enable,
        scope: ActivationScope::Global,
        keymaps: KeymapOptIn::Recommended,
    };
    let layer = PartialKoshiConfig {
        plugins: Some(PartialPluginActivationConfig {
            entries: Some(vec![entry.clone()]),
        }),
        ..Default::default()
    };
    let merged = merge(KoshiConfig::default(), vec![layer]);

    assert_eq!(merged.plugins.entries, vec![entry]);
}
