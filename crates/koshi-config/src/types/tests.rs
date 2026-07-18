//! Tests for the config schema defaults and color parsing.

use super::*;

use koshi_core::action::ActionRef;
use koshi_core::command::{
    ClosePaneArgs, Command, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs, NewPaneArgs,
    NewTabArgs, ResizePaneArgs, TabTarget,
};
use koshi_core::geometry::Direction;
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action, ActionArgs, DispatchPlan, ResolveError};

use crate::error::ColorParseError;
use crate::key::{parse_chord, Leader};

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
    assert_eq!(
        config.keybindings.modes.keys().collect::<Vec<_>>(),
        vec![&ModeName::new("locked"), &ModeName::new("normal")]
    );

    assert_eq!(config.layout.new_pane_direction, Direction::Right);
    assert_eq!(config.layout.default_layout, None);

    assert!(config.plugins.entries.is_empty());

    assert!(config.mouse.border_resize);
    assert_eq!(config.mouse.scroll_lines, 3);
    assert_eq!(config.mouse.wheel, WheelScroll::ScrollScrollback);

    assert!(!config.copy.copy_on_select);
    assert!(config.copy.trim_trailing_whitespace);
    assert_eq!(config.copy.clipboard, ClipboardBackend::Osc52);

    assert_eq!(config.terminal.term, "xterm-256color");
    assert_eq!(config.terminal.colorterm, "truecolor");
    assert_eq!(config.terminal.default_shell, None);

    assert_eq!(config.theme.name, "default");

    assert!(!config.logging.enabled);
}

#[test]
fn default_palette_has_expected_roles() {
    let palette = ColorPalette::default();
    assert_eq!(palette.ramp_start, RgbColor::new(0x58, 0x1c, 0x87));
    assert_eq!(palette.ramp_end, RgbColor::new(0x3b, 0x82, 0xf6));
    assert_eq!(palette.on_ramp, RgbColor::new(0xf4, 0xf1, 0xfa));
    assert_eq!(palette.on_ramp_dim, RgbColor::new(0xc9, 0xc4, 0xd4));
    assert_eq!(palette.accent, RgbColor::new(0xa7, 0x8b, 0xfa));
    assert_eq!(palette.on_accent, RgbColor::new(0x1e, 0x10, 0x33));
    assert_eq!(palette.border_focused, RgbColor::new(0x00, 0xaf, 0xd7));
    assert_eq!(palette.border_unfocused, RgbColor::new(0x58, 0x58, 0x58));
    assert_eq!(palette.border_hover, RgbColor::new(0xaf, 0x5f, 0xff));
    assert_eq!(palette.stack_header_fg, RgbColor::new(0xf4, 0xf1, 0xfa));
    assert_eq!(palette.stack_header_bg, RgbColor::new(0x30, 0x0f, 0x4a));
    assert_eq!(palette.letterbox, RgbColor::new(0x58, 0x58, 0x58));
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
fn from_hex_rejects_empty_value() {
    assert_eq!(
        RgbColor::from_hex(""),
        Err(ColorParseError::BadLength { got: 0 })
    );
    assert_eq!(
        RgbColor::from_hex("#"),
        Err(ColorParseError::BadLength { got: 0 })
    );
}

#[test]
fn from_hex_rejects_too_long_value() {
    assert_eq!(
        RgbColor::from_hex("#1234567"),
        Err(ColorParseError::BadLength { got: 7 })
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
fn from_hex_classifies_a_six_character_multibyte_value_as_bad_digit() {
    // "12345é" is exactly six characters (the é is multi-byte), so the
    // documented length rule passes and the non-hex `é` is the real fault.
    assert_eq!(
        RgbColor::from_hex("12345\u{e9}"),
        Err(ColorParseError::BadDigit {
            value: "12345\u{e9}".to_string()
        })
    );
}

#[test]
fn from_hex_counts_length_in_characters_for_multibyte_values() {
    // "café" is four characters (five bytes); the reported length matches
    // what the user typed, not the byte count.
    assert_eq!(
        RgbColor::from_hex("caf\u{e9}"),
        Err(ColorParseError::BadLength { got: 4 })
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

/// One expected default binding: where it lives, what it binds, and the exact
/// outcome of resolving it against the built-in action registry.
struct ExpectedBinding {
    mode: &'static str,
    chord: &'static str,
    action: &'static str,
    args: ActionArgs,
    resolved: Result<Command, ResolveError>,
}

/// The complete expected default binding table, row for row.
fn expected_default_bindings() -> Vec<ExpectedBinding> {
    let row = |mode: &'static str,
               chord: &'static str,
               action: &'static str,
               args: ActionArgs,
               resolved: Result<Command, ResolveError>| ExpectedBinding {
        mode,
        chord,
        action,
        args,
        resolved,
    };
    let focus_cmd = |direction: Direction| {
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Direction(direction),
            client: None,
        })
    };
    let resize_cmd = |direction: Direction| {
        Command::ResizePane(ResizePaneArgs {
            pane: None,
            direction,
            size: 1,
        })
    };
    let new_pane_cmd = |direction: Direction| {
        Command::NewPane(NewPaneArgs {
            source: None,
            direction: Some(direction),
            stacked: false,
            cwd: None,
            command: None,
            client: None,
        })
    };

    vec![
        row(
            "normal",
            "<C-l>",
            "lock",
            ActionArgs::None,
            Ok(Command::SetLockMode(LockModeArgs { locked: true })),
        ),
        row(
            "normal",
            "<C-q>",
            "quit",
            ActionArgs::None,
            Ok(Command::Quit),
        ),
        row(
            "normal",
            "<C-p> n",
            "new-pane",
            ActionArgs::None,
            Ok(Command::NewPane(NewPaneArgs::default())),
        ),
        row(
            "normal",
            "<C-p> h",
            "new-pane-left",
            ActionArgs::None,
            Ok(new_pane_cmd(Direction::Left)),
        ),
        row(
            "normal",
            "<C-p> j",
            "new-pane-down",
            ActionArgs::None,
            Ok(new_pane_cmd(Direction::Down)),
        ),
        row(
            "normal",
            "<C-p> k",
            "new-pane-up",
            ActionArgs::None,
            Ok(new_pane_cmd(Direction::Up)),
        ),
        row(
            "normal",
            "<C-p> l",
            "new-pane-right",
            ActionArgs::None,
            Ok(new_pane_cmd(Direction::Right)),
        ),
        row(
            "normal",
            "<C-p> x",
            "close-pane-tree",
            ActionArgs::None,
            Ok(Command::ClosePane(ClosePaneArgs {
                pane: None,
                force: false,
                tree: true,
            })),
        ),
        row(
            "normal",
            "<A-f>",
            "toggle-pane-fullscreen",
            ActionArgs::None,
            Ok(Command::TogglePaneFullscreen),
        ),
        row(
            "normal",
            "<C-p> <Left>",
            "focus-pane-left",
            ActionArgs::None,
            Ok(focus_cmd(Direction::Left)),
        ),
        row(
            "normal",
            "<C-p> <Down>",
            "focus-pane-down",
            ActionArgs::None,
            Ok(focus_cmd(Direction::Down)),
        ),
        row(
            "normal",
            "<C-p> <Up>",
            "focus-pane-up",
            ActionArgs::None,
            Ok(focus_cmd(Direction::Up)),
        ),
        row(
            "normal",
            "<C-p> <Right>",
            "focus-pane-right",
            ActionArgs::None,
            Ok(focus_cmd(Direction::Right)),
        ),
        row(
            "normal",
            "<A-h>",
            "focus-pane-left",
            ActionArgs::None,
            Ok(focus_cmd(Direction::Left)),
        ),
        row(
            "normal",
            "<A-j>",
            "focus-pane-down",
            ActionArgs::None,
            Ok(focus_cmd(Direction::Down)),
        ),
        row(
            "normal",
            "<A-k>",
            "focus-pane-up",
            ActionArgs::None,
            Ok(focus_cmd(Direction::Up)),
        ),
        row(
            "normal",
            "<A-l>",
            "focus-pane-right",
            ActionArgs::None,
            Ok(focus_cmd(Direction::Right)),
        ),
        row(
            "normal",
            "<C-s> h",
            "resize-pane-left",
            ActionArgs::None,
            Ok(resize_cmd(Direction::Left)),
        ),
        row(
            "normal",
            "<C-s> j",
            "resize-pane-down",
            ActionArgs::None,
            Ok(resize_cmd(Direction::Down)),
        ),
        row(
            "normal",
            "<C-s> k",
            "resize-pane-up",
            ActionArgs::None,
            Ok(resize_cmd(Direction::Up)),
        ),
        row(
            "normal",
            "<C-s> l",
            "resize-pane-right",
            ActionArgs::None,
            Ok(resize_cmd(Direction::Right)),
        ),
        row(
            "normal",
            "<A-t>",
            "new-tab",
            ActionArgs::None,
            Ok(Command::NewTab(NewTabArgs {
                cwd: None,
                client: None,
            })),
        ),
        row(
            "normal",
            "<Tab>",
            "next-tab",
            ActionArgs::None,
            Ok(Command::FocusTab(FocusTabArgs {
                target: TabTarget::Next,
                client: None,
            })),
        ),
        row(
            "normal",
            "<S-Tab>",
            "previous-tab",
            ActionArgs::None,
            Ok(Command::FocusTab(FocusTabArgs {
                target: TabTarget::Prev,
                client: None,
            })),
        ),
        row(
            "locked",
            "<C-l>",
            "unlock",
            ActionArgs::None,
            Ok(Command::SetLockMode(LockModeArgs { locked: false })),
        ),
        row(
            "locked",
            "<C-q>",
            "quit",
            ActionArgs::None,
            Ok(Command::Quit),
        ),
    ]
}

#[test]
fn default_binding_table_is_exact_and_resolves() {
    let config = KoshiConfig::default();
    let registry = ActionRegistry::new();
    let expected = expected_default_bindings();

    let total: usize = config
        .keybindings
        .modes
        .values()
        .map(|bindings| bindings.keys.len())
        .sum();
    assert_eq!(total, expected.len());

    for row in expected {
        // Space-separated single chords; each token parses on its own (the
        // multi-chord grammar itself belongs to the sequence parser).
        let mut chords = row
            .chord
            .split(' ')
            .map(|token| parse_chord(token).expect("default chord text parses"));
        let first = chords.next().expect("expected chord text is non-empty");
        let sequence = KeySequence::new(first, chords.collect());
        let bound = config
            .keybindings
            .modes
            .get(&ModeName::new(row.mode))
            .expect("default mode exists")
            .keys
            .get(&sequence)
            .unwrap_or_else(|| panic!("no default binding on {} in {}", row.chord, row.mode));
        assert_eq!(
            bound.action,
            ActionRef::core(row.action).expect("expected action name is valid"),
            "action bound to {}",
            row.chord
        );
        assert_eq!(bound.args, row.args, "args bound to {}", row.chord);
        assert_eq!(
            resolve_action(&bound.action, &bound.args, &registry),
            row.resolved.map(DispatchPlan::Command),
            "resolution of {}",
            row.chord
        );
    }
}

#[test]
fn default_bindings_open_non_typeable_and_skip_ambiguous_ctrl_chords() {
    let config = KoshiConfig::default();
    // On unix terminals without the kitty keyboard protocol these four Ctrl
    // chords arrive as the Tab, Enter, Esc, and Backspace control bytes.
    let ambiguous = ['i', 'm', '[', 'h'].map(|c| KeyChord::new(ModFlags::CTRL, Key::Char(c)));
    // The owner-chosen exception to the non-typeable-opening rule
    // (2026-07-12): tab switching is the bare Tab / Shift+Tab pair; a shell
    // sees a literal Tab only while the client is locked.
    let tab_switch = [
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Tab)),
        KeyChord::new(ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
    ];
    for (mode, bindings) in &config.keybindings.modes {
        for sequence in bindings.keys.keys() {
            // Only the OPENING chord competes with plain typing; later
            // chords are read while the pending sequence is live.
            let first = &sequence.chords()[0];
            assert!(
                !first.is_typeable() || tab_switch.contains(first),
                "default {first} in {mode:?} opens with a typeable chord"
            );
            for chord in sequence.chords() {
                assert!(
                    !ambiguous.contains(chord),
                    "default {chord} in {mode:?} is ambiguous without the kitty protocol"
                );
            }
        }
    }
}

#[test]
fn reserved_unlock_is_the_locked_mode_binding() {
    let config = KoshiConfig::default();
    assert_eq!(KeybindingsConfig::RESERVED_UNLOCK.to_string(), "<C-l>");
    assert_eq!(
        parse_chord("<C-l>").expect("reserved unlock text parses"),
        KeybindingsConfig::RESERVED_UNLOCK
    );

    let locked = &config.keybindings.modes[&ModeName::new("locked")];
    // The reserved unlock — the same chord normal mode locks with, so one
    // key flips both ways — plus the quit chord.
    assert_eq!(locked.keys.len(), 2);
    let bound = locked
        .keys
        .get(&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK))
        .expect("locked mode binds the reserved unlock chord");
    assert_eq!(
        bound.action,
        ActionRef::core("unlock").expect("unlock name is valid")
    );
    assert_eq!(bound.args, ActionArgs::None);
}

#[test]
fn prefix_labels_name_exactly_the_default_prefix_chords() {
    let labels = default_prefix_labels();
    assert_eq!(labels.len(), 2);
    assert_eq!(
        labels
            .get(&KeyChord::new(ModFlags::CTRL, Key::Char('p')))
            .map(String::as_str),
        Some("PANE")
    );
    assert_eq!(
        labels
            .get(&KeyChord::new(ModFlags::CTRL, Key::Char('s')))
            .map(String::as_str),
        Some("RESIZE")
    );

    // Every labeled chord opens at least one multi-chord default sequence,
    // and every multi-chord default sequence's opening chord is labeled —
    // the label table and the binding table stay in lockstep.
    let normal = &default_mode_bindings()[&ModeName::new("normal")];
    let openers: std::collections::BTreeSet<KeyChord> = normal
        .keys
        .keys()
        .filter(|sequence| sequence.chords().len() > 1)
        .map(|sequence| sequence.chords()[0])
        .collect();
    assert_eq!(openers, labels.keys().copied().collect());
}
