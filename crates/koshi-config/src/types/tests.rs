//! Tests for the config schema defaults, the built-in binding table and its
//! prefix labels, and color parsing.

use super::*;

use koshi_core::action::{ActionReference, ClientActionKind};
use koshi_core::command::{
    ClosePaneArgs, CloseTabArgs, Command, FocusPaneArgs, FocusTabArgs, FocusTarget, LockModeArgs,
    NewPaneArgs, NewTabArgs, ResizePaneArgs, TabTarget,
};
use koshi_core::geometry::Direction;
use koshi_core::key::{
    BindingModifierFlags, ExtendedKeysMode, Key, KeyChord, KeySequence, NamedKey,
};
use koshi_core::log::{LogFormat, LogLevel};
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action, DispatchPlan, ResolveError};

use crate::error::ColorParseError;
use crate::key::{parse_chord, Leader};

/// The binding of `core:<action_name>` with no arguments.
fn build_bound_action(action_name: &str) -> BoundAction {
    BoundAction {
        action_reference: ActionReference::from_core_action_name(action_name)
            .expect("expected action name is valid"),
    }
}

#[test]
fn default_server_and_client_configs_match_expected_settings() {
    let server_config = ServerConfig::default();
    let client_config = ClientConfig::default();

    assert_eq!(server_config.config_schema_version, SCHEMA_VERSION);
    assert_eq!(client_config.config_schema_version, SCHEMA_VERSION);

    assert_eq!(server_config.pane.minimum_column_count, 2);
    assert_eq!(server_config.pane.minimum_row_count, 1);
    assert_eq!(server_config.pane.gap_cell_count, 0);

    assert_eq!(server_config.scrollback.maximum_line_count, 10_000);
    assert_eq!(
        server_config.scrollback.maximum_byte_count,
        32 * 1024 * 1024
    );
    assert!(client_config.scrollback.should_scroll_to_input);

    assert!(!server_config.should_allow_beta_features);
    assert!(!server_config.should_allow_other_users);
    assert_eq!(server_config.remote_listen_address, None);
    assert_eq!(server_config.shared_sessions_directory, None);
    assert!(!server_config.should_auto_close_session);
    assert!(client_config.supports_image_protocols);
    assert!(!client_config.should_reduce_motion);
    assert!(client_config.should_stay_in_pane_placement_mode_after_placement);
    assert!(client_config.should_reconnect_remote_session);

    assert_eq!(client_config.keybindings.chord_timeout_ms, 500);
    assert_eq!(client_config.keybindings.which_key_delay_ms, 300);
    assert_eq!(client_config.keybindings.maximum_chord_depth, 4);
    assert_eq!(
        client_config.keybindings.leader,
        Leader::Modifiers(BindingModifierFlags::CTRL)
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
            &ModeName::from_text("normal"),
            &ModeName::from_text("pane-placement")
        ]
    );

    assert_eq!(client_config.layout.new_pane_direction, Direction::Right);

    assert!(client_config.mouse.can_resize_pane_border);
    assert_eq!(client_config.mouse.scroll_line_count, 3);
    assert_eq!(
        client_config.mouse.wheel_scroll,
        WheelScroll::ScrollScrollback
    );

    assert!(client_config.copy.should_trim_trailing_whitespace);

    assert_eq!(server_config.terminal.term, "xterm-256color");
    assert_eq!(server_config.terminal.colorterm, "truecolor");
    assert_eq!(server_config.terminal.default_shell, None);
    assert_eq!(
        server_config.terminal.extended_keys_mode,
        ExtendedKeysMode::OnRequest
    );

    assert_eq!(client_config.theme.theme_name, "default");
    assert_eq!(client_config.theme.colors, ColorPalette::default());

    assert!(client_config.update.should_auto_check_for_updates);
    assert_eq!(client_config.update.check_interval_days, 14);
    assert!(!client_config.update.should_allow_prerelease_updates);

    // Logging is process-local: both sides carry the same defaults.
    assert_eq!(server_config.logging, LoggingConfig::default());
    assert_eq!(client_config.logging, LoggingConfig::default());
    assert!(!server_config.logging.is_enabled);
    assert_eq!(server_config.logging.log_level, LogLevel::Warning);
    assert_eq!(server_config.logging.log_format, LogFormat::Pretty);
    assert!(!client_config.logging.is_enabled);
}

#[test]
fn default_palette_has_expected_roles() {
    let default_palette = ColorPalette::default();
    assert_eq!(
        default_palette.ramp_start,
        RgbColor::from_channels(0xd0, 0xa5, 0xff)
    );
    assert_eq!(
        default_palette.ramp_end,
        RgbColor::from_channels(0x7d, 0xbc, 0xff)
    );
    assert_eq!(
        default_palette.on_ramp,
        RgbColor::from_channels(0x12, 0x09, 0x1f)
    );
    assert_eq!(
        default_palette.on_ramp_dim,
        RgbColor::from_channels(0xf0, 0xec, 0xfa)
    );
    assert_eq!(
        default_palette.accent,
        RgbColor::from_channels(0xf5, 0xc2, 0xff)
    );
    assert_eq!(
        default_palette.on_accent,
        RgbColor::from_channels(0x1e, 0x10, 0x33)
    );
    assert_eq!(
        default_palette.border_focused,
        RgbColor::from_channels(0x00, 0xaf, 0xd7)
    );
    assert_eq!(
        default_palette.border_unfocused,
        RgbColor::from_channels(0x58, 0x58, 0x58)
    );
    assert_eq!(
        default_palette.border_hover,
        RgbColor::from_channels(0xaf, 0x5f, 0xff)
    );
    assert_eq!(
        default_palette.stack_header_fg,
        RgbColor::from_channels(0xf4, 0xf1, 0xfa)
    );
    assert_eq!(
        default_palette.stack_header_bg,
        RgbColor::from_channels(0x30, 0x0f, 0x4a)
    );
    assert_eq!(
        default_palette.letterbox,
        RgbColor::from_channels(0x58, 0x58, 0x58)
    );
    assert_eq!(
        default_palette.bar_bg,
        RgbColor::from_channels(0x00, 0x00, 0x00)
    );
}

#[test]
fn from_hex_parses_color_text_with_leading_hash() {
    assert_eq!(
        RgbColor::from_hex("#00afd7"),
        Ok(RgbColor::from_channels(0x00, 0xaf, 0xd7))
    );
}

#[test]
fn from_hex_parses_uppercase_color_text_without_hash() {
    assert_eq!(
        RgbColor::from_hex("00AFD7"),
        Ok(RgbColor::from_channels(0x00, 0xaf, 0xd7))
    );
}

#[test]
fn from_hex_rejects_color_text_with_wrong_length() {
    assert_eq!(
        RgbColor::from_hex("#fff"),
        Err(ColorParseError::BadLength { character_count: 3 })
    );
}

#[test]
fn from_hex_rejects_empty_color_text() {
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
fn from_hex_rejects_color_text_longer_than_six_characters() {
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
fn from_hex_rejects_a_non_hex_character_in_six_character_multibyte_text() {
    // "12345é" is six characters, and `é` is two bytes. The length check
    // passes, and the digit check refuses `é`.
    assert_eq!(
        RgbColor::from_hex("12345\u{e9}"),
        Err(ColorParseError::BadDigit {
            invalid_hex_text: "12345\u{e9}".to_string()
        })
    );
}

#[test]
fn from_hex_counts_color_text_characters_instead_of_bytes() {
    // "café" is four characters and five bytes: the reported length is 4.
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
fn from_hex_keeps_surrounding_whitespace_in_color_text() {
    // Nothing is trimmed. With a leading space, the `#` is not at the front:
    // all eight characters are measured.
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
fn from_hex_strips_only_one_leading_hash_from_color_text() {
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
    // A CSS-style name is not hex. "red" is three characters: the length
    // check refuses it before the digit check.
    assert_eq!(
        RgbColor::from_hex("red"),
        Err(ColorParseError::BadLength { character_count: 3 })
    );
    // "orange" is six characters: it passes the length check and fails the
    // digit check.
    assert_eq!(
        RgbColor::from_hex("orange"),
        Err(ColorParseError::BadDigit {
            invalid_hex_text: "orange".to_string()
        })
    );
}

#[test]
fn from_hex_counts_a_lone_hash_as_empty_color_text() {
    // Stripping the single `#` leaves an empty value, reported as zero digits.
    assert_eq!(
        RgbColor::from_hex("#"),
        Err(ColorParseError::BadLength { character_count: 0 })
    );
    // Only the leading `#` is stripped. A trailing `#` stays as content: the
    // value is six characters with one non-hex digit.
    assert_eq!(
        RgbColor::from_hex("#12345#"),
        Err(ColorParseError::BadDigit {
            invalid_hex_text: "12345#".to_string()
        })
    );
}

#[test]
fn mode_name_text_roundtrips() {
    let resize_mode_name = ModeName::from_text("resize");
    assert_eq!(resize_mode_name.get_name(), "resize");
}

#[test]
fn mode_names_compare_by_exact_text() {
    // The map key is the raw string. Case and surrounding space both count: a
    // `mode "Normal"` block is a different mode from `mode "normal"`.
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

    // Ordering is the string ordering: `locked` sorts before `normal` in every
    // `BTreeMap<ModeName, _>`.
    assert!(ModeName::from_text("locked") < ModeName::from_text("normal"));
}

#[test]
fn mode_name_maps_answer_string_lookups() {
    // `ModeName` borrows as `str`: a `BTreeMap<ModeName, _>` answers a `&str`
    // key exactly as it answers the owned key.
    let integer_by_mode_name = BTreeMap::from([
        (ModeName::from_text("locked"), 1),
        (ModeName::from_text("normal"), 2),
    ]);
    assert_eq!(integer_by_mode_name.get("locked"), Some(&1));
    assert_eq!(integer_by_mode_name.get("normal"), Some(&2));
    assert_eq!(integer_by_mode_name.get("Normal"), None);
    assert_eq!(integer_by_mode_name.get("normal "), None);
    assert_eq!(integer_by_mode_name.get(""), None);
}

#[test]
fn the_default_modes_remove_no_sequences() {
    let default_mode_bindings_by_name = build_default_mode_bindings(Leader::default());
    for mode_name in ["normal", "locked", "pane-placement"] {
        assert_eq!(
            default_mode_bindings_by_name[&ModeName::from_text(mode_name)].removed_key_sequences,
            BTreeSet::new(),
            "{mode_name} mode removes nothing"
        );
    }
}

#[test]
fn an_empty_mode_binds_and_removes_nothing() {
    let empty_mode_bindings = ModeBindings::default();
    assert_eq!(
        empty_mode_bindings.bound_action_by_key_sequence,
        BTreeMap::new()
    );
    assert_eq!(empty_mode_bindings.removed_key_sequences, BTreeSet::new());
}

#[test]
fn the_default_theme_is_named_default() {
    assert_eq!(DEFAULT_THEME_NAME, "default");
    assert_eq!(ThemeConfig::default().theme_name, DEFAULT_THEME_NAME);
}

#[test]
fn the_default_keybindings_hold_the_table_for_their_own_leader() {
    let keybindings_config = KeybindingsConfig::default();
    assert_eq!(
        keybindings_config.mode_bindings_by_name,
        build_default_mode_bindings(keybindings_config.leader)
    );
}

#[test]
fn each_default_mode_binds_every_action_to_one_key() {
    for (mode_name, mode_bindings) in build_default_mode_bindings(Leader::default()) {
        let mut action_references: Vec<String> = mode_bindings
            .bound_action_by_key_sequence
            .values()
            .map(|bound_action| bound_action.action_reference.to_string())
            .collect();
        let bound_key_count = action_references.len();
        action_references.sort();
        action_references.dedup();
        assert_eq!(
            action_references.len(),
            bound_key_count,
            "{mode_name:?} binds one action to two keys"
        );
    }
}

#[test]
fn wheel_scroll_and_leader_defaults_match_expected_variants() {
    assert_eq!(WheelScroll::default(), WheelScroll::ScrollScrollback);
    assert_eq!(
        Leader::default(),
        Leader::Modifiers(BindingModifierFlags::CTRL)
    );
}

#[test]
fn a_shift_modifier_leader_moves_every_leader_binding() {
    // `parse_leader("S-")` yields this leader. Shift merges onto the lowercase
    // letters the defaults use, and the whole table builds: `<leader>q`
    // becomes `<S-q>`, and `<leader>p n` becomes `<S-p> n`.
    let mode_bindings_by_name =
        build_default_mode_bindings(Leader::Modifiers(BindingModifierFlags::SHIFT));
    let normal_mode_bindings = &mode_bindings_by_name[&ModeName::from_text("normal")];

    assert_eq!(normal_mode_bindings.bound_action_by_key_sequence.len(), 24);
    assert_eq!(
        normal_mode_bindings.bound_action_by_key_sequence[&KeySequence::from(
            KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Char('q'))
        )],
        build_bound_action("quit")
    );
    assert_eq!(
        normal_mode_bindings.bound_action_by_key_sequence[&KeySequence::from_first_and_rest(
            KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Char('p')),
            vec![KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Char('n')
            )],
        )],
        build_bound_action("new-pane")
    );
    // `<S-Tab>` is written literally: it stays `<S-Tab>`, apart from the
    // moved `<S-t>` prefix.
    assert_eq!(
        normal_mode_bindings.bound_action_by_key_sequence[&KeySequence::from(
            KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Named(NamedKey::Tab))
        )],
        build_bound_action("previous-tab")
    );
}

#[test]
fn a_chord_leader_prefixes_the_locked_bindings_and_leaves_the_unlock_chord() {
    let space_leader_key_chord =
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Space));
    let mode_bindings_by_name = build_default_mode_bindings(Leader::Chord(space_leader_key_chord));
    let locked_mode_bindings = &mode_bindings_by_name[&ModeName::from_text("locked")];
    let build_sequence_after_leader = |key| {
        KeySequence::from_first_and_rest(
            space_leader_key_chord,
            vec![KeyChord::from_parts(BindingModifierFlags::NONE, key)],
        )
    };

    assert_eq!(locked_mode_bindings.bound_action_by_key_sequence.len(), 4);
    // The reserved unlock is written literally: it stays `<C-l>`.
    assert_eq!(
        locked_mode_bindings.bound_action_by_key_sequence
            [&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)],
        build_bound_action("unlock")
    );
    assert_eq!(
        locked_mode_bindings.bound_action_by_key_sequence
            [&build_sequence_after_leader(Key::Char('q'))],
        build_bound_action("quit")
    );
    assert_eq!(
        locked_mode_bindings.bound_action_by_key_sequence
            [&build_sequence_after_leader(Key::Char('g'))],
        build_bound_action("mouse-select")
    );
    assert_eq!(
        locked_mode_bindings.bound_action_by_key_sequence[&KeySequence::from_first_and_rest(
            space_leader_key_chord,
            vec![
                KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('p')),
                KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('m')),
            ],
        )],
        build_bound_action("begin-pane-placement")
    );
}

#[test]
fn a_leader_chord_that_is_also_a_binding_keeps_both() {
    // `<A-f>` is the fullscreen binding and, here, the leader as well. The
    // one-chord sequence and the sequences it opens are separate map keys:
    // the table holds all 24 normal-mode bindings.
    let fullscreen_key_chord = KeyChord::from_parts(BindingModifierFlags::ALT, Key::Char('f'));
    let mode_bindings_by_name = build_default_mode_bindings(Leader::Chord(fullscreen_key_chord));
    let normal_mode_bindings = &mode_bindings_by_name[&ModeName::from_text("normal")];

    assert_eq!(normal_mode_bindings.bound_action_by_key_sequence.len(), 24);
    assert_eq!(
        normal_mode_bindings.bound_action_by_key_sequence[&KeySequence::from(fullscreen_key_chord)],
        build_bound_action("toggle-pane-fullscreen")
    );
    assert_eq!(
        normal_mode_bindings.bound_action_by_key_sequence[&KeySequence::from_first_and_rest(
            fullscreen_key_chord,
            vec![KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Char('q')
            )]
        )],
        build_bound_action("quit")
    );
}

/// The `layout.new-pane-direction` the resolving client holds while the default
/// binding table below is checked: `Up`. The stock default is `Right`, and the
/// `new-pane` rows resolve to `Up`.
const CLIENT_SPLIT_DIRECTION: Direction = Direction::Up;

/// One expected default binding: where it lives, what it binds, and the exact
/// outcome of resolving it against the built-in action registry.
struct ExpectedBinding {
    mode_name: &'static str,
    key_sequence_text: &'static str,
    action_name: &'static str,
    resolved_dispatch: Result<Command, ResolveError>,
    client_action: Option<ClientActionKind>,
}

/// The complete expected default binding table, binding by binding.
fn list_expected_default_bindings() -> Vec<ExpectedBinding> {
    let build_expected_binding =
        |mode_name: &'static str,
         key_sequence_text: &'static str,
         action_name: &'static str,
         resolved_dispatch: Result<Command, ResolveError>| ExpectedBinding {
            mode_name,
            key_sequence_text,
            action_name,
            resolved_dispatch,
            client_action: None,
        };
    let build_expected_client_binding =
        |mode_name: &'static str,
         key_sequence_text: &'static str,
         action_name: &'static str,
         client_action: ClientActionKind| ExpectedBinding {
            mode_name,
            key_sequence_text,
            action_name,
            resolved_dispatch: Err(ResolveError::ArgumentsRequired {
                action_reference: ActionReference::from_core_action_name(action_name)
                    .expect("expected action name is valid"),
            }),
            client_action: Some(client_action),
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
            Ok(Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None,
            })),
        ),
        build_expected_binding("normal", "<C-q>", "quit", Ok(Command::Quit)),
        build_expected_binding(
            "normal",
            "<C-p> n",
            "new-pane",
            Ok(build_new_pane_command(CLIENT_SPLIT_DIRECTION)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> h",
            "new-pane-left",
            Ok(build_new_pane_command(Direction::Left)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> j",
            "new-pane-down",
            Ok(build_new_pane_command(Direction::Down)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> k",
            "new-pane-up",
            Ok(build_new_pane_command(Direction::Up)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> l",
            "new-pane-right",
            Ok(build_new_pane_command(Direction::Right)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> s",
            "new-pane-stacked",
            Ok(Command::NewPane(NewPaneArgs {
                source_pane_id: None,
                tab_id: None,
                direction: CLIENT_SPLIT_DIRECTION,
                should_stack: true,
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            })),
        ),
        build_expected_client_binding(
            "normal",
            "<C-p> m",
            "begin-pane-placement",
            ClientActionKind::BeginPanePlacement,
        ),
        build_expected_binding(
            "normal",
            "<C-p> x",
            "close-pane-tree",
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
            Ok(Command::TogglePaneFullscreen),
        ),
        build_expected_binding(
            "normal",
            "<C-p> <Left>",
            "focus-pane-left",
            Ok(build_focus_command(Direction::Left)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> <Down>",
            "focus-pane-down",
            Ok(build_focus_command(Direction::Down)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> <Up>",
            "focus-pane-up",
            Ok(build_focus_command(Direction::Up)),
        ),
        build_expected_binding(
            "normal",
            "<C-p> <Right>",
            "focus-pane-right",
            Ok(build_focus_command(Direction::Right)),
        ),
        build_expected_binding(
            "normal",
            "<C-s> <Left>",
            "resize-pane-left",
            Ok(build_resize_command(Direction::Left)),
        ),
        build_expected_binding(
            "normal",
            "<C-s> <Down>",
            "resize-pane-down",
            Ok(build_resize_command(Direction::Down)),
        ),
        build_expected_binding(
            "normal",
            "<C-s> <Up>",
            "resize-pane-up",
            Ok(build_resize_command(Direction::Up)),
        ),
        build_expected_binding(
            "normal",
            "<C-s> <Right>",
            "resize-pane-right",
            Ok(build_resize_command(Direction::Right)),
        ),
        build_expected_binding(
            "normal",
            "<C-t> n",
            "new-tab",
            Ok(Command::NewTab(NewTabArgs {
                working_directory: None,
                client_id: None,
            })),
        ),
        build_expected_binding(
            "normal",
            "<C-t> x",
            "close-tab",
            Ok(Command::CloseTab(CloseTabArgs::default())),
        ),
        build_expected_binding(
            "normal",
            "<Tab>",
            "next-tab",
            Ok(Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Next,
                client_id: None,
            })),
        ),
        build_expected_binding(
            "normal",
            "<S-Tab>",
            "previous-tab",
            Ok(Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Previous,
                client_id: None,
            })),
        ),
        build_expected_binding(
            "locked",
            "<C-l>",
            "unlock",
            Ok(Command::SetLockMode(LockModeArgs {
                is_locked: false,
                client_id: None,
            })),
        ),
        build_expected_client_binding(
            "locked",
            "<C-p> m",
            "begin-pane-placement",
            ClientActionKind::BeginPanePlacement,
        ),
        build_expected_binding("locked", "<C-q>", "quit", Ok(Command::Quit)),
        build_expected_client_binding(
            "pane-placement",
            "<Left>",
            "select-pane-target-left",
            ClientActionKind::SelectPaneTarget(Direction::Left),
        ),
        build_expected_client_binding(
            "pane-placement",
            "<Down>",
            "select-pane-target-down",
            ClientActionKind::SelectPaneTarget(Direction::Down),
        ),
        build_expected_client_binding(
            "pane-placement",
            "<Up>",
            "select-pane-target-up",
            ClientActionKind::SelectPaneTarget(Direction::Up),
        ),
        build_expected_client_binding(
            "pane-placement",
            "<Right>",
            "select-pane-target-right",
            ClientActionKind::SelectPaneTarget(Direction::Right),
        ),
        build_expected_client_binding(
            "pane-placement",
            "<S-Left>",
            "select-pane-insertion-left",
            ClientActionKind::SelectPaneInsertion(Direction::Left),
        ),
        build_expected_client_binding(
            "pane-placement",
            "<S-Down>",
            "select-pane-insertion-down",
            ClientActionKind::SelectPaneInsertion(Direction::Down),
        ),
        build_expected_client_binding(
            "pane-placement",
            "<S-Up>",
            "select-pane-insertion-up",
            ClientActionKind::SelectPaneInsertion(Direction::Up),
        ),
        build_expected_client_binding(
            "pane-placement",
            "<S-Right>",
            "select-pane-insertion-right",
            ClientActionKind::SelectPaneInsertion(Direction::Right),
        ),
        build_expected_client_binding(
            "pane-placement",
            "<Space>",
            "cycle-pane-placement-span",
            ClientActionKind::CyclePanePlacementSpan,
        ),
        build_expected_client_binding(
            "pane-placement",
            "<Tab>",
            "select-next-placement-tab",
            ClientActionKind::SelectNextPlacementTab,
        ),
        build_expected_client_binding(
            "pane-placement",
            "<S-Tab>",
            "select-previous-placement-tab",
            ClientActionKind::SelectPreviousPlacementTab,
        ),
        build_expected_client_binding(
            "pane-placement",
            "<CR>",
            "confirm-pane-placement",
            ClientActionKind::ConfirmPanePlacement,
        ),
        build_expected_client_binding(
            "pane-placement",
            "<Esc>",
            "cancel-pane-placement",
            ClientActionKind::CancelPanePlacement,
        ),
        build_expected_binding(
            "normal",
            "<C-g>",
            "mouse-select",
            Ok(Command::ToggleMouseSelect),
        ),
        build_expected_binding(
            "locked",
            "<C-g>",
            "mouse-select",
            Ok(Command::ToggleMouseSelect),
        ),
    ]
}

#[test]
fn default_keybinding_table_matches_expected_actions_and_dispatches() {
    let client_config = ClientConfig::default();
    let action_registry = ActionRegistry::new();
    let expected_binding_rows = list_expected_default_bindings();

    let default_binding_count: usize = client_config
        .keybindings
        .mode_bindings_by_name
        .values()
        .map(|mode_bindings| mode_bindings.bound_action_by_key_sequence.len())
        .sum();
    assert_eq!(default_binding_count, expected_binding_rows.len());

    for expected_binding in expected_binding_rows {
        // `key_sequence_text` is space-separated chords. Each chord parses on
        // its own.
        let mut parsed_chords = expected_binding
            .key_sequence_text
            .split(' ')
            .map(|chord_text| parse_chord(chord_text).expect("default chord text parses"));
        let first_chord = parsed_chords
            .next()
            .expect("expected chord text is non-empty");
        let key_sequence = KeySequence::from_first_and_rest(first_chord, parsed_chords.collect());
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
        let actual_dispatch = resolve_action(
            &bound_action.action_reference,
            &action_registry,
            CLIENT_SPLIT_DIRECTION,
        );
        if let Some(client_action) = expected_binding.client_action {
            assert_eq!(
                actual_dispatch,
                Ok(DispatchPlan::ClientAction(client_action)),
                "resolution of {}",
                expected_binding.key_sequence_text
            );
        } else {
            assert_eq!(
                actual_dispatch,
                expected_binding
                    .resolved_dispatch
                    .map(|command| DispatchPlan::Command(Box::new(command))),
                "resolution of {}",
                expected_binding.key_sequence_text
            );
        }
    }
}

#[test]
fn default_keybindings_use_non_typeable_openers_and_avoid_ambiguous_ctrl_chords() {
    let client_config = ClientConfig::default();
    // On unix terminals without the kitty keyboard protocol these four Ctrl
    // chords arrive as the Tab, Enter, Esc, and Backspace control bytes.
    let ambiguous_control_chords = ['i', 'm', '[', 'h'].map(|key_character| {
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char(key_character))
    });
    // The one exception to the non-typeable-opening rule: tab switching is the
    // bare Tab / Shift+Tab pair, and a shell sees a literal Tab only while the
    // client is locked.
    let tab_switch_chords = [
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Tab)),
        KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Named(NamedKey::Tab)),
    ];
    for (mode_name, mode_bindings) in &client_config.keybindings.mode_bindings_by_name {
        if mode_name.get_name() == "pane-placement" {
            continue;
        }
        for key_sequence in mode_bindings.bound_action_by_key_sequence.keys() {
            // Only the OPENING chord competes with plain typing; subsequent
            // chords are read while the pending sequence is live.
            let opening_chord = &key_sequence.list_chords()[0];
            assert!(
                !opening_chord.is_typeable() || tab_switch_chords.contains(opening_chord),
                "default {opening_chord} in {mode_name:?} opens with a typeable chord"
            );
            for key_chord in key_sequence.list_chords() {
                assert!(
                    !ambiguous_control_chords.contains(key_chord),
                    "default {key_chord} in {mode_name:?} is ambiguous without the kitty protocol"
                );
            }
        }
    }
}

#[test]
fn reserved_unlock_is_the_locked_mode_binding() {
    let client_config = ClientConfig::default();
    assert_eq!(KeybindingsConfig::RESERVED_UNLOCK.to_string(), "<C-l>");
    assert_eq!(
        parse_chord("<C-l>").expect("reserved unlock text parses"),
        KeybindingsConfig::RESERVED_UNLOCK
    );

    let locked_mode_bindings =
        &client_config.keybindings.mode_bindings_by_name[&ModeName::from_text("locked")];
    // The reserved unlock, the pane placement opener, quit, and mouse-select.
    assert_eq!(locked_mode_bindings.bound_action_by_key_sequence.len(), 4);
    let reserved_unlock_bound_action = locked_mode_bindings
        .bound_action_by_key_sequence
        .get(&KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK))
        .expect("locked mode binds the reserved unlock chord");
    assert_eq!(
        reserved_unlock_bound_action.action_reference,
        ActionReference::from_core_action_name("unlock").expect("unlock name is valid")
    );
}

#[test]
fn prefix_labels_name_exactly_the_default_prefix_chords() {
    let default_prefix_labels = build_default_prefix_labels(Leader::default());
    assert_eq!(default_prefix_labels.len(), 3);
    assert_eq!(
        default_prefix_labels
            .get(&KeyChord::from_parts(
                BindingModifierFlags::CTRL,
                Key::Char('p')
            ))
            .map(String::as_str),
        Some("PANE")
    );
    assert_eq!(
        default_prefix_labels
            .get(&KeyChord::from_parts(
                BindingModifierFlags::CTRL,
                Key::Char('s')
            ))
            .map(String::as_str),
        Some("RESIZE")
    );
    assert_eq!(
        default_prefix_labels
            .get(&KeyChord::from_parts(
                BindingModifierFlags::CTRL,
                Key::Char('t')
            ))
            .map(String::as_str),
        Some("TAB")
    );

    // Every labeled chord opens at least one multi-chord default sequence,
    // and every multi-chord default sequence's opening chord is labeled.
    let normal_mode_bindings =
        &build_default_mode_bindings(Leader::default())[&ModeName::from_text("normal")];
    let opening_chords: BTreeSet<KeyChord> = normal_mode_bindings
        .bound_action_by_key_sequence
        .keys()
        .filter(|key_sequence| key_sequence.list_chords().len() > 1)
        .map(|key_sequence| key_sequence.list_chords()[0])
        .collect();
    assert_eq!(
        opening_chords,
        default_prefix_labels.keys().copied().collect()
    );
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
                BindingModifierFlags::NONE,
                Key::Char(key_character),
            )],
        )
    };

    // Default leader (the Ctrl modifier run): `<leader>p n` is `<C-p> n`.
    let control_leader_bindings = build_normal_mode_bindings(Leader::default());
    assert_eq!(
        control_leader_bindings[&build_two_key_sequence(BindingModifierFlags::CTRL, 'p', 'n')],
        build_bound_action("new-pane")
    );
    assert_eq!(
        control_leader_bindings[&build_single_key_sequence(BindingModifierFlags::CTRL, 'g')],
        build_bound_action("mouse-select")
    );

    // Rebind the leader to Alt: the same defaults become `<A-p> n` / `<A-g>`,
    // and the Ctrl forms are gone.
    let alternate_leader_bindings =
        build_normal_mode_bindings(Leader::Modifiers(BindingModifierFlags::ALT));
    assert_eq!(
        alternate_leader_bindings[&build_two_key_sequence(BindingModifierFlags::ALT, 'p', 'n')],
        build_bound_action("new-pane")
    );
    assert_eq!(
        alternate_leader_bindings[&build_single_key_sequence(BindingModifierFlags::ALT, 'g')],
        build_bound_action("mouse-select")
    );
    assert_eq!(
        alternate_leader_bindings.get(&build_two_key_sequence(
            BindingModifierFlags::CTRL,
            'p',
            'n'
        )),
        None
    );

    // A chord leader (Space) makes the leader a prefix: `<Space> p n`.
    let space_leader_bindings = build_normal_mode_bindings(Leader::Chord(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(NamedKey::Space),
    )));
    let space_leader_pane_creation_sequence = KeySequence::from_first_and_rest(
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Space)),
        vec![
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('p')),
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('n')),
        ],
    );
    assert_eq!(
        space_leader_bindings[&space_leader_pane_creation_sequence],
        build_bound_action("new-pane")
    );

    // Explicit bindings never move: `<A-f>` and the reserved `<C-l>` are the
    // same under every leader.
    let fullscreen_key_sequence = build_single_key_sequence(BindingModifierFlags::ALT, 'f');
    let reserved_unlock_key_sequence = KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK);
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
            mode_bindings[&reserved_unlock_key_sequence],
            build_bound_action("lock"),
            "reserved <C-l> never moves"
        );
    }
}

#[test]
fn a_chord_leader_drops_the_ambiguous_prefix_labels() {
    // A chord leader opens every leader binding with the leader chord:
    // `<leader>p`, `<leader>s`, and `<leader>t` share one opening chord, and
    // the label map is empty.
    let space_leader_prefix_labels = build_default_prefix_labels(Leader::Chord(
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Space)),
    ));
    assert!(space_leader_prefix_labels.is_empty());

    // A modifier-run leader keeps `<leader>p`, `<leader>s`, and `<leader>t` at
    // distinct opening chords: all three labels stand, on Alt.
    let alt_leader_prefix_labels =
        build_default_prefix_labels(Leader::Modifiers(BindingModifierFlags::ALT));
    let find_alt_prefix_label = |key_character| {
        alt_leader_prefix_labels
            .get(&KeyChord::from_parts(
                BindingModifierFlags::ALT,
                Key::Char(key_character),
            ))
            .map(String::as_str)
    };
    assert_eq!(alt_leader_prefix_labels.len(), 3);
    assert_eq!(find_alt_prefix_label('p'), Some("PANE"));
    assert_eq!(find_alt_prefix_label('s'), Some("RESIZE"));
    assert_eq!(find_alt_prefix_label('t'), Some("TAB"));
}

#[test]
fn this_build_writes_config_schema_version_two() {
    // Every config file this build writes carries schema version 2.
    assert_eq!(SCHEMA_VERSION, 2);
}
