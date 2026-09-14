//! Tests for the config schema defaults, the built-in binding table and its
//! prefix labels, and color parsing.

use super::*;

use koshi_core::action::ActionReference;
use koshi_core::command::{
    ClosePaneArgs, CloseTabArgs, Command, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs,
    NewPaneArgs, NewTabArgs, ResizePaneArgs, TabTarget,
};
use koshi_core::geometry::Direction;
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey};
use koshi_core::log::{LogFormat, LogLevel};
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action, ActionArgs, DispatchPlan, ResolveError};

use crate::error::ColorParseError;
use crate::key::{parse_chord, Leader};

/// The argless `core:<name>` binding the default table stores.
fn build_bound_action(action_name: &str) -> BoundAction {
    BoundAction {
        action_reference: ActionReference::from_core_action_name(action_name)
            .expect("expected action name is valid"),
        action_arguments: ActionArgs::None,
    }
}

#[test]
fn default_loads_with_expected_values() {
    let server = ServerConfig::default();
    let client_config = ClientConfig::default();

    assert_eq!(server.config_schema_version, SCHEMA_VERSION);
    assert_eq!(client_config.config_schema_version, SCHEMA_VERSION);

    assert_eq!(server.pane.minimum_column_count, 2);
    assert_eq!(server.pane.minimum_row_count, 1);
    assert_eq!(server.pane.gap_cell_count, 0);

    assert_eq!(server.scrollback.maximum_line_count, 10_000);
    assert_eq!(server.scrollback.maximum_byte_count, 32 * 1024 * 1024);
    assert!(client_config.scrollback.should_scroll_to_input);

    assert!(!server.should_allow_beta_features);
    assert!(!server.should_allow_other_users);
    assert_eq!(server.remote_listen, None);
    assert_eq!(server.shared_sessions_directory, None);
    assert!(!server.should_auto_close_session);
    assert!(client_config.supports_image_protocols);
    assert!(client_config.should_reconnect_remote_session);

    assert_eq!(client_config.keybindings.chord_timeout_ms, 500);
    assert_eq!(client_config.keybindings.which_key_delay_ms, 300);
    assert_eq!(client_config.keybindings.max_chord_depth, 4);
    assert_eq!(
        client_config.keybindings.leader,
        Leader::Mods(ModFlags::CTRL)
    );
    assert_eq!(client_config.keybindings.unlock_alternative, None);
    assert_eq!(
        client_config
            .keybindings
            .mode_bindings_by_name
            .keys()
            .collect::<Vec<_>>(),
        vec![
            &ModeName::from_text("locked"),
            &ModeName::from_text("normal")
        ]
    );

    assert_eq!(client_config.layout.new_pane_direction, Direction::Right);

    assert!(client_config.mouse.can_resize_pane_border);
    assert_eq!(client_config.mouse.scroll_line_count, 3);
    assert_eq!(client_config.mouse.wheel, WheelScroll::ScrollScrollback);

    assert!(client_config.copy.should_copy_on_select);
    assert!(client_config.copy.should_trim_trailing_whitespace);
    assert_eq!(client_config.copy.clipboard, ClipboardBackend::Osc52);

    assert_eq!(server.terminal.term, "xterm-256color");
    assert_eq!(server.terminal.colorterm, "truecolor");
    assert_eq!(server.terminal.default_shell, None);

    assert_eq!(client_config.theme.theme_name, "default");
    assert_eq!(client_config.theme.colors, ColorPalette::default());

    assert!(client_config.update.should_auto_check_for_updates);
    assert_eq!(client_config.update.check_interval_days, 14);
    assert!(!client_config.update.should_allow_prerelease_updates);

    // Logging is process-local: both sides carry the same defaults.
    assert_eq!(server.logging, LoggingConfig::default());
    assert_eq!(client_config.logging, LoggingConfig::default());
    assert!(!server.logging.is_enabled);
    assert_eq!(server.logging.level, LogLevel::Warning);
    assert_eq!(server.logging.log_format, LogFormat::Pretty);
    assert!(!client_config.logging.is_enabled);
}

#[test]
fn default_palette_has_expected_roles() {
    let palette = ColorPalette::default();
    assert_eq!(
        palette.ramp_start,
        RgbColor::from_channels(0xd0, 0xa5, 0xff)
    );
    assert_eq!(palette.ramp_end, RgbColor::from_channels(0x7d, 0xbc, 0xff));
    assert_eq!(palette.on_ramp, RgbColor::from_channels(0x12, 0x09, 0x1f));
    assert_eq!(
        palette.on_ramp_dim,
        RgbColor::from_channels(0xf0, 0xec, 0xfa)
    );
    assert_eq!(palette.accent, RgbColor::from_channels(0xf5, 0xc2, 0xff));
    assert_eq!(palette.on_accent, RgbColor::from_channels(0x1e, 0x10, 0x33));
    assert_eq!(
        palette.border_focused,
        RgbColor::from_channels(0x00, 0xaf, 0xd7)
    );
    assert_eq!(
        palette.border_unfocused,
        RgbColor::from_channels(0x58, 0x58, 0x58)
    );
    assert_eq!(
        palette.border_hover,
        RgbColor::from_channels(0xaf, 0x5f, 0xff)
    );
    assert_eq!(
        palette.stack_header_fg,
        RgbColor::from_channels(0xf4, 0xf1, 0xfa)
    );
    assert_eq!(
        palette.stack_header_bg,
        RgbColor::from_channels(0x30, 0x0f, 0x4a)
    );
    assert_eq!(palette.letterbox, RgbColor::from_channels(0x58, 0x58, 0x58));
    assert_eq!(palette.bar_bg, RgbColor::from_channels(0x00, 0x00, 0x00));
}

#[test]
fn from_hex_parses_leading_hash() {
    assert_eq!(
        RgbColor::from_hex("#00afd7"),
        Ok(RgbColor::from_channels(0x00, 0xaf, 0xd7))
    );
}

#[test]
fn from_hex_parses_bare_and_uppercase() {
    assert_eq!(
        RgbColor::from_hex("00AFD7"),
        Ok(RgbColor::from_channels(0x00, 0xaf, 0xd7))
    );
}

#[test]
fn from_hex_rejects_wrong_length() {
    assert_eq!(
        RgbColor::from_hex("#fff"),
        Err(ColorParseError::BadLength { character_count: 3 })
    );
}

#[test]
fn from_hex_rejects_empty_value() {
    assert_eq!(
        RgbColor::from_hex(""),
        Err(ColorParseError::BadLength { character_count: 0 })
    );
    assert_eq!(
        RgbColor::from_hex("#"),
        Err(ColorParseError::BadLength { character_count: 0 })
    );
}

#[test]
fn from_hex_rejects_too_long_value() {
    assert_eq!(
        RgbColor::from_hex("#1234567"),
        Err(ColorParseError::BadLength { character_count: 7 })
    );
}

#[test]
fn from_hex_rejects_non_hex_digit() {
    assert_eq!(
        RgbColor::from_hex("#gggggg"),
        Err(ColorParseError::BadDigit {
            invalid_hex_text: "gggggg".to_string()
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
            invalid_hex_text: "12345\u{e9}".to_string()
        })
    );
}

#[test]
fn from_hex_counts_length_in_characters_for_multibyte_values() {
    // "café" is four characters (five bytes); the reported length matches
    // what the user typed, not the byte count.
    assert_eq!(
        RgbColor::from_hex("caf\u{e9}"),
        Err(ColorParseError::BadLength { character_count: 4 })
    );
}

#[test]
fn from_str_delegates_to_from_hex() {
    assert_eq!(
        "#123456".parse::<RgbColor>(),
        Ok(RgbColor::from_channels(0x12, 0x34, 0x56))
    );
}

#[test]
fn from_str_reports_the_same_errors_as_from_hex() {
    assert_eq!(
        "#fff".parse::<RgbColor>(),
        Err(ColorParseError::BadLength { character_count: 3 })
    );
    assert_eq!(
        "gggggg".parse::<RgbColor>(),
        Err(ColorParseError::BadDigit {
            invalid_hex_text: "gggggg".to_string()
        })
    );
}

#[test]
fn from_hex_keeps_surrounding_whitespace_as_content() {
    // Nothing is trimmed. A leading space keeps the `#` off the front, so the
    // whole eight-character run is measured.
    assert_eq!(
        RgbColor::from_hex(" #ffffff"),
        Err(ColorParseError::BadLength { character_count: 8 })
    );
    // A trailing space keeps the length at six and fails on the space itself.
    assert_eq!(
        RgbColor::from_hex("#fffff "),
        Err(ColorParseError::BadDigit {
            invalid_hex_text: "fffff ".to_string()
        })
    );
}

#[test]
fn from_hex_strips_only_one_leading_hash() {
    // A second `#` stays as content: the value measures seven characters.
    assert_eq!(
        RgbColor::from_hex("##ffffff"),
        Err(ColorParseError::BadLength { character_count: 7 })
    );
}

#[test]
fn from_hex_parses_the_channel_boundaries() {
    // Pure black and pure white are the two extreme channel values, with and
    // without the leading hash.
    assert_eq!(
        RgbColor::from_hex("#000000"),
        Ok(RgbColor::from_channels(0, 0, 0))
    );
    assert_eq!(
        RgbColor::from_hex("000000"),
        Ok(RgbColor::from_channels(0, 0, 0))
    );
    assert_eq!(
        RgbColor::from_hex("#ffffff"),
        Ok(RgbColor::from_channels(0xff, 0xff, 0xff))
    );
    // Upper-case and mixed-case digits fold to the same value.
    assert_eq!(
        RgbColor::from_hex("#FFFFFF"),
        Ok(RgbColor::from_channels(0xff, 0xff, 0xff))
    );
    assert_eq!(
        RgbColor::from_hex("#FfAa00"),
        Ok(RgbColor::from_channels(0xff, 0xaa, 0x00))
    );
}

#[test]
fn from_hex_rejects_a_named_color_word_by_its_length() {
    // A CSS-style name is not hex; "red" is three characters, so it fails the
    // length rule first, never reaching the digit check.
    assert_eq!(
        RgbColor::from_hex("red"),
        Err(ColorParseError::BadLength { character_count: 3 })
    );
    // "orange" is six characters, so it passes the length rule and fails on
    // the first non-hex digit instead.
    assert_eq!(
        RgbColor::from_hex("orange"),
        Err(ColorParseError::BadDigit {
            invalid_hex_text: "orange".to_string()
        })
    );
}

#[test]
fn from_hex_treats_a_lone_hash_length_as_zero() {
    // Stripping the single `#` leaves an empty value, reported as zero digits.
    assert_eq!(
        RgbColor::from_hex("#"),
        Err(ColorParseError::BadLength { character_count: 0 })
    );
    // Only the leading `#` is stripped: a trailing `#` stays as content, so
    // the value is six characters with one non-hex digit.
    assert_eq!(
        RgbColor::from_hex("#12345#"),
        Err(ColorParseError::BadDigit {
            invalid_hex_text: "12345#".to_string()
        })
    );
}

#[test]
fn mode_name_roundtrips() {
    let resize_mode_name = ModeName::from_text("resize");
    assert_eq!(resize_mode_name.get_name(), "resize");
}

#[test]
fn mode_names_compare_by_exact_text() {
    // The map key is the raw string, so case and surrounding space both count:
    // a `mode "Normal"` block is a different mode from `mode "normal"`.
    assert_eq!(
        ModeName::from_text("normal"),
        ModeName::from_text(String::from("normal"))
    );
    assert_ne!(ModeName::from_text("Normal"), ModeName::from_text("normal"));
    assert_ne!(
        ModeName::from_text("normal "),
        ModeName::from_text("normal")
    );
    assert_eq!(ModeName::from_text("").get_name(), "");

    // Ordering is the string ordering, which is what fixes the mode order in
    // every `BTreeMap<ModeName, _>`.
    assert!(ModeName::from_text("locked") < ModeName::from_text("normal"));
}

#[test]
fn mode_name_maps_answer_str_lookups() {
    // `ModeName` borrows as `str`, so a `BTreeMap<ModeName, _>` answers a
    // `&str` key exactly as it answers the owned key.
    let map = BTreeMap::from([
        (ModeName::from_text("locked"), 1),
        (ModeName::from_text("normal"), 2),
    ]);
    assert_eq!(map.get("locked"), Some(&1));
    assert_eq!(map.get("normal"), Some(&2));
    assert_eq!(map.get("Normal"), None);
    assert_eq!(map.get("normal "), None);
    assert_eq!(map.get(""), None);
}

#[test]
fn the_default_modes_remove_no_sequences() {
    let modes = build_default_mode_bindings(Leader::default());
    for mode_name in ["normal", "locked"] {
        assert_eq!(
            modes[&ModeName::from_text(mode_name)].removed_key_sequences,
            BTreeSet::new(),
            "{mode_name} mode removes nothing"
        );
    }
}

#[test]
fn an_empty_mode_binds_and_removes_nothing() {
    let empty = ModeBindings::default();
    assert_eq!(empty.bound_action_by_key_sequence, BTreeMap::new());
    assert_eq!(empty.removed_key_sequences, BTreeSet::new());
}

#[test]
fn the_default_theme_is_named_default() {
    assert_eq!(DEFAULT_THEME, "default");
    assert_eq!(ThemeConfig::default().theme_name, DEFAULT_THEME);
}

#[test]
fn the_default_keybindings_hold_the_table_for_their_own_leader() {
    let keybindings = KeybindingsConfig::default();
    assert_eq!(
        keybindings.mode_bindings_by_name,
        build_default_mode_bindings(keybindings.leader)
    );
}

#[test]
fn each_default_mode_binds_every_action_to_one_key() {
    for (mode_name, mode_bindings) in build_default_mode_bindings(Leader::default()) {
        let mut action_references: Vec<String> = mode_bindings
            .bound_action_by_key_sequence
            .values()
            .map(|binding| binding.action_reference.to_string())
            .collect();
        let key_count = action_references.len();
        action_references.sort();
        action_references.dedup();
        assert_eq!(
            action_references.len(),
            key_count,
            "{mode_name:?} binds one action to two keys"
        );
    }
}

#[test]
fn enum_defaults_are_the_shipped_variants() {
    assert_eq!(WheelScroll::default(), WheelScroll::ScrollScrollback);
    assert_eq!(ClipboardBackend::default(), ClipboardBackend::Osc52);
    assert_eq!(Leader::default(), Leader::Mods(ModFlags::CTRL));
}

#[test]
fn a_shift_modifier_leader_moves_every_leader_binding() {
    // `parse_leader("S-")` yields this leader, and merging Shift onto the
    // lowercase letters the defaults use is legal, so the whole table builds:
    // `<leader>q` becomes `<S-q>`, `<leader>p n` becomes `<S-p> n`.
    let modes = build_default_mode_bindings(Leader::Mods(ModFlags::SHIFT));
    let normal = &modes[&ModeName::from_text("normal")];

    assert_eq!(normal.bound_action_by_key_sequence.len(), 22);
    assert_eq!(
        normal.bound_action_by_key_sequence
            [&KeySequence::from(KeyChord::from_parts(ModFlags::SHIFT, Key::Char('q')))],
        build_bound_action("quit")
    );
    assert_eq!(
        normal.bound_action_by_key_sequence[&KeySequence::from_first_and_rest(
            KeyChord::from_parts(ModFlags::SHIFT, Key::Char('p')),
            vec![KeyChord::from_parts(ModFlags::NONE, Key::Char('n'))],
        )],
        build_bound_action("new-pane")
    );
    // `<S-Tab>` is written literally, so it stays put and does not collide
    // with the moved `<leader>t` prefix.
    assert_eq!(
        normal.bound_action_by_key_sequence[&KeySequence::from(KeyChord::from_parts(
            ModFlags::SHIFT,
            Key::Named(NamedKey::Tab)
        ))],
        build_bound_action("previous-tab")
    );
}

#[test]
fn a_chord_leader_prefixes_the_locked_bindings_and_leaves_the_unlock_chord() {
    let space = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Space));
    let modes = build_default_mode_bindings(Leader::Chord(space));
    let locked = &modes[&ModeName::from_text("locked")];
    let build_sequence_after_leader = |key| {
        KeySequence::from_first_and_rest(space, vec![KeyChord::from_parts(ModFlags::NONE, key)])
    };

    assert_eq!(locked.bound_action_by_key_sequence.len(), 3);
    // The reserved unlock is written literally, so it stays `<C-l>`.
    assert_eq!(
        locked.bound_action_by_key_sequence[&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        build_bound_action("unlock")
    );
    assert_eq!(
        locked.bound_action_by_key_sequence[&build_sequence_after_leader(Key::Char('q'))],
        build_bound_action("quit")
    );
    assert_eq!(
        locked.bound_action_by_key_sequence[&build_sequence_after_leader(Key::Char('g'))],
        build_bound_action("mouse-select")
    );
}

#[test]
fn a_leader_chord_that_is_also_a_binding_keeps_both() {
    // `<A-f>` is the fullscreen binding and, here, the leader as well. The
    // one-chord sequence and the sequences it opens are separate map keys, so
    // the table still holds all 22 normal-mode bindings.
    let fullscreen = KeyChord::from_parts(ModFlags::ALT, Key::Char('f'));
    let modes = build_default_mode_bindings(Leader::Chord(fullscreen));
    let normal = &modes[&ModeName::from_text("normal")];

    assert_eq!(normal.bound_action_by_key_sequence.len(), 22);
    assert_eq!(
        normal.bound_action_by_key_sequence[&KeySequence::from(fullscreen)],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(
        normal.bound_action_by_key_sequence[&KeySequence::from_first_and_rest(
            fullscreen,
            vec![KeyChord::from_parts(ModFlags::NONE, Key::Char('q'))]
        )],
        build_bound_action("quit")
    );
}

/// The `layout.new-pane-direction` the resolving client holds while the default
/// binding table below is checked. `Up`, not the stock `Right`, so the
/// `new-pane` row shows that resolution reads the client's own direction.
const CLIENT_SPLIT_DIRECTION: Direction = Direction::Up;

/// One expected default binding: where it lives, what it binds, and the exact
/// outcome of resolving it against the built-in action registry.
struct ExpectedBinding {
    mode_name: &'static str,
    key_sequence_text: &'static str,
    action_name: &'static str,
    action_arguments: ActionArgs,
    resolved_dispatch: Result<Command, ResolveError>,
}

/// The complete expected default binding table, binding by binding.
fn expected_default_bindings() -> Vec<ExpectedBinding> {
    let build_expected_binding =
        |mode_name: &'static str,
         key_sequence_text: &'static str,
         action_name: &'static str,
         action_arguments: ActionArgs,
         resolved_dispatch: Result<Command, ResolveError>| ExpectedBinding {
            mode_name,
            key_sequence_text,
            action_name,
            action_arguments,
            resolved_dispatch,
        };
    let build_focus_command = |direction: Direction| {
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Direction(direction),
            client_id: None,
        })
    };
    let build_resize_command = |direction: Direction| {
        Command::ResizePane(ResizePaneArgs {
            pane_id: None,
            direction,
            resize_amount_cells: 1,
        })
    };
    let build_new_pane_command = |direction: Direction| {
        Command::NewPane(NewPaneArgs {
            source_pane_id: None,
            tab_id: None,
            direction,
            should_stack: false,
            working_directory: None,
            spawn_spec: None,
            client_id: None,
        })
    };

    vec![
        build_expected_binding(
            "normal",
            "<C-l>",
            "lock",
            ActionArgs::None,
            Ok(Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None,
            })),
        ),
        build_expected_binding(
            "normal",
            "<C-q>",
            "quit",
            ActionArgs::None,
            Ok(Command::Quit),
        ),
        build_expected_binding(
            "normal",
            "<C-p> n",
            "new-pane",
            ActionArgs::None,
            Ok(build_new_pane_command(CLIENT_SPLIT_DIRECTION)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> h",
            "new-pane-left",
            ActionArgs::None,
            Ok(build_new_pane_command(Direction::Left)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> j",
            "new-pane-down",
            ActionArgs::None,
            Ok(build_new_pane_command(Direction::Down)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> k",
            "new-pane-up",
            ActionArgs::None,
            Ok(build_new_pane_command(Direction::Up)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> l",
            "new-pane-right",
            ActionArgs::None,
            Ok(build_new_pane_command(Direction::Right)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> x",
            "close-pane-tree",
            ActionArgs::None,
            Ok(Command::ClosePane(ClosePaneArgs {
                pane_id: None,
                should_force_close: false,
                should_kill_process_tree: true,
            })),
        ),
        build_expected_binding(
            "normal",
            "<A-f>",
            "toggle-pane-fullscreen",
            ActionArgs::None,
            Ok(Command::TogglePaneFullscreen),
        ),
        build_expected_binding(
            "normal",
            "<C-p> <Left>",
            "focus-pane-left",
            ActionArgs::None,
            Ok(build_focus_command(Direction::Left)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> <Down>",
            "focus-pane-down",
            ActionArgs::None,
            Ok(build_focus_command(Direction::Down)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> <Up>",
            "focus-pane-up",
            ActionArgs::None,
            Ok(build_focus_command(Direction::Up)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> <Right>",
            "focus-pane-right",
            ActionArgs::None,
            Ok(build_focus_command(Direction::Right)),
        ),
        build_expected_binding(
            "normal",
            "<C-s> <Left>",
            "resize-pane-left",
            ActionArgs::None,
            Ok(build_resize_command(Direction::Left)),
        ),
        build_expected_binding(
            "normal",
            "<C-s> <Down>",
            "resize-pane-down",
            ActionArgs::None,
            Ok(build_resize_command(Direction::Down)),
        ),
        build_expected_binding(
            "normal",
            "<C-s> <Up>",
            "resize-pane-up",
            ActionArgs::None,
            Ok(build_resize_command(Direction::Up)),
        ),
        build_expected_binding(
            "normal",
            "<C-s> <Right>",
            "resize-pane-right",
            ActionArgs::None,
            Ok(build_resize_command(Direction::Right)),
        ),
        build_expected_binding(
            "normal",
            "<C-t> n",
            "new-tab",
            ActionArgs::None,
            Ok(Command::NewTab(NewTabArgs {
                working_directory: None,
                client_id: None,
            })),
        ),
        build_expected_binding(
            "normal",
            "<C-t> x",
            "close-tab",
            ActionArgs::None,
            Ok(Command::CloseTab(CloseTabArgs::default())),
        ),
        build_expected_binding(
            "normal",
            "<Tab>",
            "next-tab",
            ActionArgs::None,
            Ok(Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Next,
                client_id: None,
            })),
        ),
        build_expected_binding(
            "normal",
            "<S-Tab>",
            "previous-tab",
            ActionArgs::None,
            Ok(Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Prev,
                client_id: None,
            })),
        ),
        build_expected_binding(
            "locked",
            "<C-l>",
            "unlock",
            ActionArgs::None,
            Ok(Command::SetLockMode(LockModeArgs {
                is_locked: false,
                client_id: None,
            })),
        ),
        build_expected_binding(
            "locked",
            "<C-q>",
            "quit",
            ActionArgs::None,
            Ok(Command::Quit),
        ),
        build_expected_binding(
            "normal",
            "<C-g>",
            "mouse-select",
            ActionArgs::None,
            Ok(Command::ToggleMouseSelect),
        ),
        build_expected_binding(
            "locked",
            "<C-g>",
            "mouse-select",
            ActionArgs::None,
            Ok(Command::ToggleMouseSelect),
        ),
    ]
}

#[test]
fn default_binding_table_is_exact_and_resolves() {
    let client_config = ClientConfig::default();
    let registry = ActionRegistry::new();
    let expected_binding_rows = expected_default_bindings();

    let binding_count: usize = client_config
        .keybindings
        .mode_bindings_by_name
        .values()
        .map(|bindings| bindings.bound_action_by_key_sequence.len())
        .sum();
    assert_eq!(binding_count, expected_binding_rows.len());

    for expected_binding in expected_binding_rows {
        // Space-separated single chords; each token parses on its own (the
        // multi-chord grammar itself belongs to the sequence parser).
        let mut chord_tokens = expected_binding
            .key_sequence_text
            .split(' ')
            .map(|token| parse_chord(token).expect("default chord text parses"));
        let first_chord = chord_tokens
            .next()
            .expect("expected chord text is non-empty");
        let key_sequence = KeySequence::from_first_and_rest(first_chord, chord_tokens.collect());
        let bound_action = client_config
            .keybindings
            .mode_bindings_by_name
            .get(&ModeName::from_text(expected_binding.mode_name))
            .expect("default mode exists")
            .bound_action_by_key_sequence
            .get(&key_sequence)
            .unwrap_or_else(|| {
                panic!(
                    "no default binding on {} in {}",
                    expected_binding.key_sequence_text, expected_binding.mode_name
                )
            });
        assert_eq!(
            bound_action.action_reference,
            ActionReference::from_core_action_name(expected_binding.action_name)
                .expect("expected action name is valid"),
            "action bound to {}",
            expected_binding.key_sequence_text
        );
        assert_eq!(
            bound_action.action_arguments, expected_binding.action_arguments,
            "action arguments bound to {}",
            expected_binding.key_sequence_text
        );
        assert_eq!(
            resolve_action(
                &bound_action.action_reference,
                &bound_action.action_arguments,
                &registry,
                CLIENT_SPLIT_DIRECTION,
            ),
            expected_binding
                .resolved_dispatch
                .map(DispatchPlan::Command),
            "resolution of {}",
            expected_binding.key_sequence_text
        );
    }
}

#[test]
fn default_bindings_open_non_typeable_and_skip_ambiguous_ctrl_chords() {
    let client_config = ClientConfig::default();
    // On unix terminals without the kitty keyboard protocol these four Ctrl
    // chords arrive as the Tab, Enter, Esc, and Backspace control bytes.
    let ambiguous_control_chords = ['i', 'm', '[', 'h']
        .map(|character| KeyChord::from_parts(ModFlags::CTRL, Key::Char(character)));
    // The one exception to the non-typeable-opening rule: tab switching is the
    // bare Tab / Shift+Tab pair, and a shell sees a literal Tab only while the
    // client is locked.
    let tab_switch_chords = [
        KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Tab)),
        KeyChord::from_parts(ModFlags::SHIFT, Key::Named(NamedKey::Tab)),
    ];
    for (mode_name, mode_bindings) in &client_config.keybindings.mode_bindings_by_name {
        for key_sequence in mode_bindings.bound_action_by_key_sequence.keys() {
            // Only the OPENING chord competes with plain typing; subsequent
            // chords are read while the pending sequence is live.
            let opening_chord = &key_sequence.list_chords()[0];
            assert!(
                !opening_chord.is_typeable() || tab_switch_chords.contains(opening_chord),
                "default {opening_chord} in {mode_name:?} opens with a typeable chord"
            );
            for chord in key_sequence.list_chords() {
                assert!(
                    !ambiguous_control_chords.contains(chord),
                    "default {chord} in {mode_name:?} is ambiguous without the kitty protocol"
                );
            }
        }
    }
}

#[test]
fn reserved_unlock_is_the_locked_mode_binding() {
    let config = ClientConfig::default();
    assert_eq!(KeybindingsConfig::RESERVED_UNLOCK.to_string(), "<C-l>");
    assert_eq!(
        parse_chord("<C-l>").expect("reserved unlock text parses"),
        KeybindingsConfig::RESERVED_UNLOCK
    );

    let locked = &config.keybindings.mode_bindings_by_name[&ModeName::from_text("locked")];
    // The reserved unlock — the same chord normal mode locks with, so one
    // key flips both ways — plus the quit and mouse-select chords, which fire
    // whether or not the client is locked.
    assert_eq!(locked.bound_action_by_key_sequence.len(), 3);
    let bound_action = locked
        .bound_action_by_key_sequence
        .get(&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK))
        .expect("locked mode binds the reserved unlock chord");
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("unlock").expect("unlock name is valid")
    );
    assert_eq!(bound_action.action_arguments, ActionArgs::None);
}

#[test]
fn prefix_labels_name_exactly_the_default_prefix_chords() {
    let labels = default_prefix_labels(Leader::default());
    assert_eq!(labels.len(), 3);
    assert_eq!(
        labels
            .get(&KeyChord::from_parts(ModFlags::CTRL, Key::Char('p')))
            .map(String::as_str),
        Some("PANE")
    );
    assert_eq!(
        labels
            .get(&KeyChord::from_parts(ModFlags::CTRL, Key::Char('s')))
            .map(String::as_str),
        Some("RESIZE")
    );
    assert_eq!(
        labels
            .get(&KeyChord::from_parts(ModFlags::CTRL, Key::Char('t')))
            .map(String::as_str),
        Some("TAB")
    );

    // Every labeled chord opens at least one multi-chord default sequence,
    // and every multi-chord default sequence's opening chord is labeled —
    // the label table and the binding table stay in lockstep.
    let normal = &build_default_mode_bindings(Leader::default())[&ModeName::from_text("normal")];
    let opening_chords: std::collections::BTreeSet<KeyChord> = normal
        .bound_action_by_key_sequence
        .keys()
        .filter(|sequence| sequence.list_chords().len() > 1)
        .map(|sequence| sequence.list_chords()[0])
        .collect();
    assert_eq!(opening_chords, labels.keys().copied().collect());
}

#[test]
fn default_bindings_follow_the_leader() {
    let build_normal_mode_bindings = |leader| {
        build_default_mode_bindings(leader)[&ModeName::from_text("normal")]
            .bound_action_by_key_sequence
            .clone()
    };
    let build_single_key_sequence = |modifier_flags, key_character| {
        KeySequence::from(KeyChord::from_parts(
            modifier_flags,
            Key::Char(key_character),
        ))
    };
    let build_two_key_sequence = |modifier_flags, prefix_character, key_character| {
        KeySequence::from_first_and_rest(
            KeyChord::from_parts(modifier_flags, Key::Char(prefix_character)),
            vec![KeyChord::from_parts(
                ModFlags::NONE,
                Key::Char(key_character),
            )],
        )
    };

    // Default leader (the Ctrl modifier run): `<leader>p n` is `<C-p> n`.
    let control_leader_bindings = build_normal_mode_bindings(Leader::default());
    assert_eq!(
        control_leader_bindings[&build_two_key_sequence(ModFlags::CTRL, 'p', 'n')],
        build_bound_action("new-pane")
    );
    assert_eq!(
        control_leader_bindings[&build_single_key_sequence(ModFlags::CTRL, 'g')],
        build_bound_action("mouse-select")
    );

    // Rebind the leader to Alt: the same defaults become `<A-p> n` / `<A-g>`,
    // and the Ctrl forms are gone.
    let alternate_leader_bindings = build_normal_mode_bindings(Leader::Mods(ModFlags::ALT));
    assert_eq!(
        alternate_leader_bindings[&build_two_key_sequence(ModFlags::ALT, 'p', 'n')],
        build_bound_action("new-pane")
    );
    assert_eq!(
        alternate_leader_bindings[&build_single_key_sequence(ModFlags::ALT, 'g')],
        build_bound_action("mouse-select")
    );
    assert_eq!(
        alternate_leader_bindings.get(&build_two_key_sequence(ModFlags::CTRL, 'p', 'n')),
        None
    );

    // A chord leader (Space) makes the leader a prefix: `<Space> p n`.
    let space_leader_bindings = build_normal_mode_bindings(Leader::Chord(KeyChord::from_parts(
        ModFlags::NONE,
        Key::Named(NamedKey::Space),
    )));
    let space_p_n = KeySequence::from_first_and_rest(
        KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Space)),
        vec![
            KeyChord::from_parts(ModFlags::NONE, Key::Char('p')),
            KeyChord::from_parts(ModFlags::NONE, Key::Char('n')),
        ],
    );
    assert_eq!(
        space_leader_bindings[&space_p_n],
        build_bound_action("new-pane")
    );

    // Explicit bindings never move: `<A-f>` and the reserved `<C-l>` are the
    // same under every leader.
    let fullscreen_key_sequence = build_single_key_sequence(ModFlags::ALT, 'f');
    let unlock = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
    for mode_bindings in [
        &control_leader_bindings,
        &alternate_leader_bindings,
        &space_leader_bindings,
    ] {
        assert_eq!(
            mode_bindings[&fullscreen_key_sequence],
            build_bound_action("toggle-pane-fullscreen"),
            "explicit <A-f> never moves"
        );
        assert_eq!(
            mode_bindings[&unlock],
            build_bound_action("lock"),
            "reserved <C-l> never moves"
        );
    }
}

#[test]
fn a_chord_leader_drops_the_ambiguous_prefix_labels() {
    // A chord leader opens every leader binding with the leader chord, so
    // `<leader>p`, `<leader>s`, and `<leader>t` share an opening — no single
    // group label fits, and the hint bar shows the derived `+N` instead.
    let space = default_prefix_labels(Leader::Chord(KeyChord::from_parts(
        ModFlags::NONE,
        Key::Named(NamedKey::Space),
    )));
    assert!(space.is_empty());

    // A modifier-run leader keeps `<leader>p`, `<leader>s`, and `<leader>t` at
    // distinct openings, so all three labels stand, moved onto Alt.
    let alt = default_prefix_labels(Leader::Mods(ModFlags::ALT));
    let alt_label = |ch| {
        alt.get(&KeyChord::from_parts(ModFlags::ALT, Key::Char(ch)))
            .map(String::as_str)
    };
    assert_eq!(alt.len(), 3);
    assert_eq!(alt_label('p'), Some("PANE"));
    assert_eq!(alt_label('s'), Some("RESIZE"));
    assert_eq!(alt_label('t'), Some("TAB"));
}

#[test]
fn this_build_writes_config_schema_version_one() {
    // `SCHEMA_VERSION` reads `koshi_core::compat::CONFIG_SCHEMA.maximum_version`, one
    // crate away. This pins the version number every config file this build
    // writes carries.
    assert_eq!(SCHEMA_VERSION, 1);
}
