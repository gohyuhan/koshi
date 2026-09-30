//! Tests for config layering: precedence, deep field-level merge, the
//! whole-value replace of collection fields, and the split that folds one
//! parsed layer onto the session and the viewer separately.

use std::collections::BTreeMap;
use std::path::PathBuf;

use koshi_core::geometry::Direction;
use koshi_core::key::{BindingModifierFlags, Key, KeyChord};

use super::*;
use crate::types::{ModeBindings, ModeName, RgbColor};

#[test]
fn default_partial_config_preserves_server_and_client_defaults() {
    let merged_server_config =
        merge_server(ServerConfig::default(), vec![PartialKoshiConfig::default()]);
    assert_eq!(merged_server_config, ServerConfig::default());

    let merged_client_config =
        merge_client(ClientConfig::default(), vec![PartialKoshiConfig::default()]);
    assert_eq!(merged_client_config, ClientConfig::default());
}

#[test]
fn no_config_layers_preserve_server_and_client_base_configs() {
    assert_eq!(
        merge_server(ServerConfig::default(), vec![]),
        ServerConfig::default()
    );
    assert_eq!(
        merge_client(ClientConfig::default(), vec![]),
        ClientConfig::default()
    );
}

#[test]
fn beta_features_are_off_unless_the_file_turns_them_on() {
    let merged_server_config =
        merge_server(ServerConfig::default(), vec![PartialKoshiConfig::default()]);
    assert!(!merged_server_config.should_allow_beta_features);
}

#[test]
fn allow_beta_features_folds_onto_the_session_side_only() {
    let layer = PartialKoshiConfig {
        should_allow_beta_features: Some(true),
        ..Default::default()
    };

    let merged_server_config = merge_server(ServerConfig::default(), vec![layer.clone()]);
    assert!(merged_server_config.should_allow_beta_features);
    assert_eq!(
        merged_server_config,
        ServerConfig {
            should_allow_beta_features: true,
            ..ServerConfig::default()
        }
    );

    // A viewer folds the same file and is untouched by it.
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);
    assert_eq!(merged_client_config, ClientConfig::default());
}

#[test]
fn a_higher_precedence_layer_can_turn_beta_features_back_off() {
    let user_layer = PartialKoshiConfig {
        should_allow_beta_features: Some(true),
        ..Default::default()
    };
    let session_layer = PartialKoshiConfig {
        should_allow_beta_features: Some(false),
        ..Default::default()
    };

    assert!(
        !merge_server(ServerConfig::default(), vec![user_layer, session_layer])
            .should_allow_beta_features
    );
}

#[test]
fn a_session_is_reachable_by_its_own_user_only_unless_the_file_opens_it() {
    // The built-in default: with no `koshi.kdl`, only the user who started a
    // session can reach it.
    let merged_server_config =
        merge_server(ServerConfig::default(), vec![PartialKoshiConfig::default()]);
    assert!(!merged_server_config.should_allow_other_users);
    assert_eq!(merged_server_config.shared_sessions_directory, None);
}

#[test]
fn allow_other_users_folds_onto_the_session_side_only() {
    let layer = PartialKoshiConfig {
        should_allow_other_users: Some(true),
        ..Default::default()
    };

    // The session side takes the value.
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer.clone()]);
    assert!(merged_server_config.should_allow_other_users);
    assert_eq!(
        merged_server_config,
        ServerConfig {
            should_allow_other_users: true,
            ..ServerConfig::default()
        }
    );

    // A viewer folds the same file and is untouched by it.
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);
    assert_eq!(merged_client_config, ClientConfig::default());
}

#[test]
fn a_higher_precedence_layer_can_shut_other_users_back_out() {
    let user_layer = PartialKoshiConfig {
        should_allow_other_users: Some(true),
        ..Default::default()
    };
    let session_layer = PartialKoshiConfig {
        should_allow_other_users: Some(false),
        ..Default::default()
    };

    assert!(
        !merge_server(ServerConfig::default(), vec![user_layer, session_layer])
            .should_allow_other_users
    );
}

#[test]
fn a_higher_precedence_layer_wins_on_the_shared_sessions_directory() {
    let user_layer = PartialKoshiConfig {
        shared_sessions_directory: Some(Some(PathBuf::from("/var/run/koshi"))),
        ..Default::default()
    };
    let session_layer = PartialKoshiConfig {
        shared_sessions_directory: Some(Some(PathBuf::from("/tmp/koshi"))),
        ..Default::default()
    };

    let merged_server_config = merge_server(
        ServerConfig::default(),
        vec![user_layer.clone(), session_layer],
    );
    assert_eq!(
        merged_server_config.shared_sessions_directory,
        Some(PathBuf::from("/tmp/koshi"))
    );

    // A viewer folds the same file and is untouched by it.
    let merged_client_config = merge_client(ClientConfig::default(), vec![user_layer]);
    assert_eq!(merged_client_config, ClientConfig::default());
}

#[test]
fn remote_listen_is_unset_without_a_configured_address() {
    // The built-in default: with no `koshi.kdl`, no listen address is set.
    let merged_server_config =
        merge_server(ServerConfig::default(), vec![PartialKoshiConfig::default()]);
    assert_eq!(merged_server_config.remote_listen, None);
}

#[test]
fn remote_listen_folds_onto_the_session_side_only() {
    let layer = PartialKoshiConfig {
        remote_listen: Some(Some("127.0.0.1:7654".to_string())),
        ..Default::default()
    };

    // The session side takes the address.
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer.clone()]);
    assert_eq!(
        merged_server_config.remote_listen,
        Some("127.0.0.1:7654".to_string())
    );
    assert_eq!(
        merged_server_config,
        ServerConfig {
            remote_listen: Some("127.0.0.1:7654".to_string()),
            ..ServerConfig::default()
        }
    );

    // A viewer folds the same file and is untouched by it.
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);
    assert_eq!(merged_client_config, ClientConfig::default());
}

#[test]
fn a_higher_precedence_layer_wins_on_the_listen_address() {
    let user_layer = PartialKoshiConfig {
        remote_listen: Some(Some("127.0.0.1:7654".to_string())),
        ..Default::default()
    };
    let session_layer = PartialKoshiConfig {
        remote_listen: Some(Some("0.0.0.0:9000".to_string())),
        ..Default::default()
    };

    let merged_server_config =
        merge_server(ServerConfig::default(), vec![user_layer, session_layer]);
    assert_eq!(
        merged_server_config.remote_listen,
        Some("0.0.0.0:9000".to_string())
    );
}

#[test]
fn a_session_stays_open_unless_the_file_closes_it() {
    // The built-in default: with no `koshi.kdl`, a session stays open after
    // its last client leaves.
    let merged_server_config =
        merge_server(ServerConfig::default(), vec![PartialKoshiConfig::default()]);
    assert!(!merged_server_config.should_auto_close_session);
}

#[test]
fn auto_close_session_folds_onto_the_session_side_only() {
    let layer = PartialKoshiConfig {
        should_auto_close_session: Some(true),
        ..Default::default()
    };

    // The session side takes the value.
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer.clone()]);
    assert!(merged_server_config.should_auto_close_session);
    assert_eq!(
        merged_server_config,
        ServerConfig {
            should_auto_close_session: true,
            ..ServerConfig::default()
        }
    );

    // A viewer folds the same file and is untouched by it.
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);
    assert_eq!(merged_client_config, ClientConfig::default());
}

#[test]
fn scrollback_line_count_override_keeps_byte_count_default() {
    let layer = PartialKoshiConfig {
        scrollback: Some(PartialScrollbackConfig {
            maximum_line_count: Some(5_000),
            maximum_byte_count: None,
            should_scroll_to_input: None,
        }),
        ..Default::default()
    };
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer]);

    assert_eq!(merged_server_config.scrollback.maximum_line_count, 5_000);
    // `maximum_byte_count` keeps its default.
    assert_eq!(
        merged_server_config.scrollback.maximum_byte_count,
        32 * 1024 * 1024
    );
}

#[test]
fn higher_precedence_layer_sets_scrollback_line_count() {
    let user_layer = PartialKoshiConfig {
        scrollback: Some(PartialScrollbackConfig {
            maximum_line_count: Some(5_000),
            maximum_byte_count: None,
            should_scroll_to_input: None,
        }),
        ..Default::default()
    };
    let session_layer = PartialKoshiConfig {
        scrollback: Some(PartialScrollbackConfig {
            maximum_line_count: Some(20_000),
            maximum_byte_count: None,
            should_scroll_to_input: None,
        }),
        ..Default::default()
    };
    let merged_server_config =
        merge_server(ServerConfig::default(), vec![user_layer, session_layer]);

    assert_eq!(merged_server_config.scrollback.maximum_line_count, 20_000);
    assert_eq!(
        merged_server_config.scrollback.maximum_byte_count,
        32 * 1024 * 1024
    );
}

#[test]
fn unset_higher_precedence_layer_keeps_middle_scrollback_line_count() {
    // The base, a middle layer that sets `scrollback.maximum_line_count`, and
    // a highest layer that sets only `pane`. The field keeps the middle
    // layer's value.
    let middle_precedence_layer = PartialKoshiConfig {
        scrollback: Some(PartialScrollbackConfig {
            maximum_line_count: Some(7_000),
            maximum_byte_count: None,
            should_scroll_to_input: None,
        }),
        ..Default::default()
    };
    let highest_precedence_layer = PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            minimum_column_count: Some(3),
            minimum_row_count: None,
            gap_cell_count: None,
        }),
        ..Default::default()
    };
    let merged_server_config = merge_server(
        ServerConfig::default(),
        vec![middle_precedence_layer, highest_precedence_layer],
    );

    assert_eq!(merged_server_config.scrollback.maximum_line_count, 7_000);
    assert_eq!(merged_server_config.pane.minimum_column_count, 3);
}

#[test]
fn pane_and_mouse_sections_from_separate_layers_combine() {
    let user_layer = PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            minimum_column_count: Some(10),
            minimum_row_count: None,
            gap_cell_count: None,
        }),
        ..Default::default()
    };
    let session_layer = PartialKoshiConfig {
        mouse: Some(PartialMouseConfig {
            scroll_line_count: Some(7),
            ..Default::default()
        }),
        ..Default::default()
    };
    let config_layers = vec![user_layer, session_layer];
    let merged_server_config = merge_server(ServerConfig::default(), config_layers.clone());
    let merged_client_config = merge_client(ClientConfig::default(), config_layers);

    assert_eq!(merged_server_config.pane.minimum_column_count, 10);
    assert_eq!(merged_server_config.pane.minimum_row_count, 1);
    assert_eq!(merged_client_config.mouse.scroll_line_count, 7);
    assert!(merged_client_config.mouse.can_resize_pane_border);
}

#[test]
fn a_layer_that_sets_the_pane_gap_overrides_the_default() {
    let layer = PartialKoshiConfig {
        pane: Some(PartialPaneConfig {
            minimum_column_count: None,
            minimum_row_count: None,
            gap_cell_count: Some(3),
        }),
        ..Default::default()
    };
    let merged_with_layer = merge_server(ServerConfig::default(), vec![layer]);
    let merged_without_layer = merge_server(ServerConfig::default(), Vec::new());

    assert_eq!(merged_with_layer.pane.gap_cell_count, 3);
    assert_eq!(merged_without_layer.pane.gap_cell_count, 0);
}

#[test]
fn copy_whitespace_and_terminal_type_overrides_keep_siblings() {
    let layer = PartialKoshiConfig {
        copy: Some(PartialCopyConfig {
            should_trim_trailing_whitespace: Some(false),
        }),
        terminal: Some(PartialTerminalConfig {
            term: Some("screen-256color".to_string()),
            colorterm: None,
            default_shell: None,
            extended_keys_mode: None,
        }),
        ..Default::default()
    };
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer.clone()]);
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);

    assert!(!merged_client_config.copy.should_trim_trailing_whitespace); // overridden to false
    assert_eq!(merged_server_config.terminal.term, "screen-256color");
    assert_eq!(merged_server_config.terminal.colorterm, "truecolor"); // default kept
}

#[test]
fn terminal_default_shell_override_sets_shell_path() {
    let layer = PartialKoshiConfig {
        terminal: Some(PartialTerminalConfig {
            term: None,
            colorterm: None,
            default_shell: Some(Some("/bin/zsh".to_string())),
            extended_keys_mode: None,
        }),
        ..Default::default()
    };
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer]);
    assert_eq!(
        merged_server_config.terminal.default_shell,
        Some("/bin/zsh".to_string())
    );
}

#[test]
fn new_pane_direction_layer_overrides_default() {
    let layer = PartialKoshiConfig {
        layout: Some(PartialLayoutDefaults {
            new_pane_direction: Some(Direction::Down),
        }),
        ..Default::default()
    };
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);

    assert_eq!(
        merged_client_config.layout.new_pane_direction,
        Direction::Down
    );
}

#[test]
fn theme_accent_override_keeps_other_color_roles() {
    let overridden_accent_color = RgbColor::from_channels(0xff, 0x00, 0x00);
    let layer = PartialKoshiConfig {
        theme: Some(PartialThemeConfig {
            theme_name: None,
            colors: Some(PartialColorPalette {
                accent: Some(overridden_accent_color),
                ..Default::default()
            }),
        }),
        ..Default::default()
    };
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);
    let default_palette = ClientConfig::default().theme.colors;

    assert_eq!(
        merged_client_config.theme.colors.accent,
        overridden_accent_color
    );
    // Every other role keeps its default.
    assert_eq!(
        merged_client_config.theme.colors.ramp_start,
        default_palette.ramp_start
    );
    assert_eq!(
        merged_client_config.theme.colors.ramp_end,
        default_palette.ramp_end
    );
    assert_eq!(merged_client_config.theme.theme_name, "default"); // sibling field kept
}

#[test]
fn logging_override_sets_enabled_level_and_format() {
    let layer = PartialKoshiConfig {
        logging: Some(PartialLoggingConfig {
            is_enabled: Some(true),
            level: Some(LogLevel::Error),
            log_format: Some(LogFormat::Json),
        }),
        ..Default::default()
    };
    // Logging is process-local: both sides read the same section, each for its
    // own log file.
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer.clone()]);
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);

    assert!(merged_server_config.logging.is_enabled);
    assert_eq!(merged_server_config.logging.level, LogLevel::Error);
    assert_eq!(merged_server_config.logging.log_format, LogFormat::Json);
    assert!(merged_client_config.logging.is_enabled);
    assert_eq!(merged_client_config.logging.level, LogLevel::Error);
    assert_eq!(merged_client_config.logging.log_format, LogFormat::Json);

    // An absent logging section leaves the defaults (disabled, warning, pretty).
    let default_server_config =
        merge_server(ServerConfig::default(), vec![PartialKoshiConfig::default()]);
    assert!(!default_server_config.logging.is_enabled);
    assert_eq!(default_server_config.logging.level, LogLevel::Warning);
    assert_eq!(default_server_config.logging.log_format, LogFormat::Pretty);
}

#[test]
fn partial_logging_config_keeps_unset_fields_at_defaults() {
    // The startup accessor: an absent section yields the built-in defaults, a
    // present one applies only its set fields and keeps the defaults for the rest.
    let empty_partial_config = PartialKoshiConfig::default();
    assert_eq!(
        empty_partial_config.get_logging_config(),
        LoggingConfig::default()
    );

    let partial_logging_config = PartialKoshiConfig {
        logging: Some(PartialLoggingConfig {
            is_enabled: Some(true),
            level: Some(LogLevel::Info),
            log_format: None,
        }),
        ..Default::default()
    };
    let resolved_logging_config = partial_logging_config.get_logging_config();
    assert!(resolved_logging_config.is_enabled);
    assert_eq!(resolved_logging_config.level, LogLevel::Info);
    assert_eq!(
        resolved_logging_config.log_format,
        LogFormat::Pretty,
        "unset field keeps the default"
    );
}

#[test]
fn keybinding_mode_bindings_replace_base_modes_wholesale() {
    let mut client_config = ClientConfig::default();
    client_config
        .keybindings
        .mode_bindings_by_name
        .insert(ModeName::from_text("normal"), ModeBindings::default());

    let mut replacement_mode_bindings = BTreeMap::new();
    replacement_mode_bindings.insert(ModeName::from_text("resize"), ModeBindings::default());
    let layer = PartialKoshiConfig {
        keybindings: Some(PartialKeybindingsConfig {
            mode_bindings_by_name: Some(replacement_mode_bindings.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged_client_config = merge_client(client_config, vec![layer]);

    // The whole map is replaced: the base's "normal" entry is gone.
    assert_eq!(
        merged_client_config.keybindings.mode_bindings_by_name,
        replacement_mode_bindings
    );
}

#[test]
fn unlock_alternative_key_chord_can_be_set_and_cleared() {
    let alternative_unlock_key_chord =
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('u'));
    let set_unlock_key_chord_layer = PartialKoshiConfig {
        keybindings: Some(PartialKeybindingsConfig {
            unlock_alternative: Some(Some(alternative_unlock_key_chord)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged_client_config =
        merge_client(ClientConfig::default(), vec![set_unlock_key_chord_layer]);
    assert_eq!(
        merged_client_config.keybindings.unlock_alternative,
        Some(alternative_unlock_key_chord)
    );

    // A higher-precedence layer can set the value back to "keep the built-in unlock key".
    let clear_unlock_key_chord_layer = PartialKoshiConfig {
        keybindings: Some(PartialKeybindingsConfig {
            unlock_alternative: Some(None),
            ..Default::default()
        }),
        ..Default::default()
    };
    let cleared_client_config =
        merge_client(merged_client_config, vec![clear_unlock_key_chord_layer]);
    assert_eq!(cleared_client_config.keybindings.unlock_alternative, None);

    // A layer that leaves the field unset keeps the base value, `None`.
    assert_eq!(
        merge_client(ClientConfig::default(), vec![PartialKoshiConfig::default()])
            .keybindings
            .unlock_alternative,
        None
    );
}

#[test]
fn keybinding_leader_override_keeps_timing_defaults() {
    let layer = PartialKoshiConfig {
        keybindings: Some(PartialKeybindingsConfig {
            leader: Some(Leader::Modifiers(BindingModifierFlags::ALT)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);

    assert_eq!(
        merged_client_config.keybindings.leader,
        Leader::Modifiers(BindingModifierFlags::ALT)
    );
    assert_eq!(merged_client_config.keybindings.chord_timeout_ms, 500); // default kept
    assert_eq!(merged_client_config.keybindings.maximum_chord_depth, 4); // default kept
}

#[test]
fn the_scrollback_section_splits_its_caps_from_its_follow_behavior() {
    // One `scrollback` block in the file, read by both sides: the caps bound
    // the buffer the session owns, `scroll-on-input` is the viewer's own
    // follow behavior.
    let layer = PartialKoshiConfig {
        scrollback: Some(PartialScrollbackConfig {
            maximum_line_count: Some(500),
            maximum_byte_count: Some(1_024),
            should_scroll_to_input: Some(false),
        }),
        ..Default::default()
    };
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer.clone()]);
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);

    assert_eq!(merged_server_config.scrollback.maximum_line_count, 500);
    assert_eq!(merged_server_config.scrollback.maximum_byte_count, 1_024);
    assert!(!merged_client_config.scrollback.should_scroll_to_input);
}

#[test]
fn each_side_folds_only_its_own_sections() {
    // One layer carrying both sides' sections folds each onto only its own
    // side: a viewer cannot set the shell a session spawns, and a session
    // cannot set a viewer's colors.
    let layer = PartialKoshiConfig {
        terminal: Some(PartialTerminalConfig {
            term: None,
            colorterm: None,
            default_shell: Some(Some("/bin/fish".to_string())),
            extended_keys_mode: None,
        }),
        theme: Some(PartialThemeConfig {
            theme_name: Some("midnight".to_string()),
            colors: None,
        }),
        ..Default::default()
    };
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer.clone()]);
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);

    // Each side took its own section.
    assert_eq!(
        merged_server_config.terminal.default_shell,
        Some("/bin/fish".to_string())
    );
    assert_eq!(merged_client_config.theme.theme_name, "midnight");

    // Neither side's untouched fields moved: the other side's section did not
    // leak in, and the section it does own kept its defaults elsewhere.
    assert_eq!(merged_server_config.terminal.term, "xterm-256color");
    assert_eq!(merged_server_config.terminal.colorterm, "truecolor");
    assert_eq!(
        merged_client_config.theme.colors,
        ClientConfig::default().theme.colors
    );
}

#[test]
fn config_layers_from_no_files_is_the_empty_default() {
    assert_eq!(
        ConfigLayers::from_config_file_layers(None, None, None),
        ConfigLayers::default()
    );
    assert_eq!(
        ConfigLayers::default().resolve_effective_client_config(),
        ClientConfig::default()
    );
}

#[test]
fn config_layers_drop_the_app_layers_theme_and_keybinding_sections() {
    // The app layer's theme and keybinding sections are dropped. With no
    // theme file and no `keybinding.kdl`, the palette and bindings stay built
    // in.
    let config_layers = ConfigLayers::from_config_file_layers(
        Some(PartialKoshiConfig {
            theme: Some(PartialThemeConfig {
                theme_name: Some("smuggled".to_string()),
                colors: Some(PartialColorPalette {
                    accent: Some(RgbColor::from_channels(1, 2, 3)),
                    ..PartialColorPalette::default()
                }),
            }),
            keybindings: Some(PartialKeybindingsConfig {
                maximum_chord_depth: Some(0),
                ..PartialKeybindingsConfig::default()
            }),
            layout: Some(PartialLayoutDefaults {
                new_pane_direction: Some(Direction::Down),
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );

    let merged_client_config = config_layers.resolve_effective_client_config();
    assert_eq!(merged_client_config.theme, ClientConfig::default().theme);
    assert_eq!(
        merged_client_config.keybindings,
        ClientConfig::default().keybindings
    );
    // Its own sections still apply.
    assert_eq!(
        merged_client_config.layout.new_pane_direction,
        Direction::Down
    );
}

#[test]
fn config_layers_let_the_theme_and_keybinding_files_win_over_the_app_layer() {
    let config_layers = ConfigLayers::from_config_file_layers(
        Some(PartialKoshiConfig {
            layout: Some(PartialLayoutDefaults {
                new_pane_direction: Some(Direction::Down),
            }),
            ..PartialKoshiConfig::default()
        }),
        Some(PartialThemeConfig {
            theme_name: Some("ocean".to_string()),
            colors: None,
        }),
        Some(PartialKeybindingsConfig {
            maximum_chord_depth: Some(4),
            ..PartialKeybindingsConfig::default()
        }),
    );

    let merged_client_config = config_layers.resolve_effective_client_config();
    assert_eq!(
        merged_client_config.layout.new_pane_direction,
        Direction::Down
    );
    assert_eq!(merged_client_config.theme.theme_name, "ocean");
    assert_eq!(merged_client_config.keybindings.maximum_chord_depth, 4);
}

#[test]
fn update_overrides_fold_onto_the_viewer_side_only() {
    let layer = PartialKoshiConfig {
        update: Some(PartialUpdateConfig {
            should_auto_check_for_updates: Some(false),
            check_interval_days: Some(30),
            should_allow_prerelease_updates: Some(true),
        }),
        ..Default::default()
    };

    // The viewer side takes the section.
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer.clone()]);
    assert!(!merged_client_config.update.should_auto_check_for_updates);
    assert_eq!(merged_client_config.update.check_interval_days, 30);
    assert!(merged_client_config.update.should_allow_prerelease_updates);

    // A session folds the same file and is untouched by it.
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer]);
    assert_eq!(merged_server_config, ServerConfig::default());
}

#[test]
fn an_update_layer_keeps_the_fields_it_leaves_unset() {
    let layer = PartialKoshiConfig {
        update: Some(PartialUpdateConfig {
            check_interval_days: Some(1),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);

    assert_eq!(merged_client_config.update.check_interval_days, 1);
    assert!(merged_client_config.update.should_auto_check_for_updates); // default kept
    assert!(!merged_client_config.update.should_allow_prerelease_updates); // default kept
}

#[test]
fn remote_reconnect_folds_onto_the_viewer_side_only() {
    // The built-in default: with no `koshi.kdl`, a viewer reconnects by
    // itself.
    assert!(ClientConfig::default().should_reconnect_remote_session);

    let layer = PartialKoshiConfig {
        should_reconnect_remote_session: Some(false),
        ..Default::default()
    };

    let merged_client_config = merge_client(ClientConfig::default(), vec![layer.clone()]);
    assert_eq!(
        merged_client_config,
        ClientConfig {
            should_reconnect_remote_session: false,
            ..ClientConfig::default()
        }
    );

    // A session folds the same file and is untouched by it.
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer]);
    assert_eq!(merged_server_config, ServerConfig::default());
}

#[test]
fn image_support_folds_onto_the_viewer_side_only() {
    assert!(ClientConfig::default().supports_image_protocols);

    let layer = PartialKoshiConfig {
        supports_image_protocols: Some(false),
        ..Default::default()
    };
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer.clone()]);
    assert!(!merged_client_config.supports_image_protocols);

    let merged_server_config = merge_server(ServerConfig::default(), vec![layer]);
    assert_eq!(merged_server_config, ServerConfig::default());
}

#[test]
fn stay_in_pane_placement_mode_after_placement_folds_onto_the_viewer_side_only() {
    assert!(ClientConfig::default().should_stay_in_pane_placement_mode_after_placement);

    let layer = PartialKoshiConfig {
        should_stay_in_pane_placement_mode_after_placement: Some(false),
        ..Default::default()
    };
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer.clone()]);
    assert!(!merged_client_config.should_stay_in_pane_placement_mode_after_placement);

    let merged_server_config = merge_server(ServerConfig::default(), vec![layer]);
    assert_eq!(merged_server_config, ServerConfig::default());
}

#[test]
fn reduced_motion_folds_onto_the_viewer_side_only() {
    assert!(!ClientConfig::default().should_reduce_motion);

    let layer = PartialKoshiConfig {
        should_reduce_motion: Some(true),
        ..Default::default()
    };
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer.clone()]);
    assert!(merged_client_config.should_reduce_motion);

    let merged_server_config = merge_server(ServerConfig::default(), vec![layer]);
    assert_eq!(merged_server_config, ServerConfig::default());
}

#[test]
fn mouse_overrides_fold_onto_the_viewer_side_only() {
    let layer = PartialKoshiConfig {
        mouse: Some(PartialMouseConfig {
            can_resize_pane_border: Some(false),
            scroll_line_count: Some(9),
            wheel: Some(WheelScroll::Ignore),
        }),
        ..Default::default()
    };

    let merged_client_config = merge_client(ClientConfig::default(), vec![layer.clone()]);
    assert!(!merged_client_config.mouse.can_resize_pane_border);
    assert_eq!(merged_client_config.mouse.scroll_line_count, 9);
    assert_eq!(merged_client_config.mouse.wheel, WheelScroll::Ignore);

    // A session folds the same file and is untouched by it.
    let merged_server_config = merge_server(ServerConfig::default(), vec![layer]);
    assert_eq!(merged_server_config, ServerConfig::default());
}

#[test]
fn keybinding_timing_overrides_leave_the_bindings_alone() {
    let layer = PartialKoshiConfig {
        keybindings: Some(PartialKeybindingsConfig {
            chord_timeout_ms: Some(1_200),
            which_key_delay_ms: Some(50),
            maximum_chord_depth: Some(7),
            ..Default::default()
        }),
        ..Default::default()
    };
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);

    assert_eq!(merged_client_config.keybindings.chord_timeout_ms, 1_200);
    assert_eq!(merged_client_config.keybindings.which_key_delay_ms, 50);
    assert_eq!(merged_client_config.keybindings.maximum_chord_depth, 7);
    assert_eq!(
        merged_client_config.keybindings.mode_bindings_by_name,
        ClientConfig::default().keybindings.mode_bindings_by_name
    );
    assert_eq!(
        merged_client_config.keybindings.leader,
        ClientConfig::default().keybindings.leader
    );
}

#[test]
fn every_color_role_can_be_overridden() {
    let palette_color = RgbColor::from_channels(0x0a, 0x0b, 0x0c);
    let layer = PartialKoshiConfig {
        theme: Some(PartialThemeConfig {
            theme_name: None,
            colors: Some(PartialColorPalette {
                ramp_start: Some(palette_color),
                ramp_end: Some(palette_color),
                on_ramp: Some(palette_color),
                on_ramp_dim: Some(palette_color),
                accent: Some(palette_color),
                on_accent: Some(palette_color),
                border_focused: Some(palette_color),
                border_unfocused: Some(palette_color),
                border_hover: Some(palette_color),
                stack_header_fg: Some(palette_color),
                stack_header_bg: Some(palette_color),
                letterbox: Some(palette_color),
                bar_bg: Some(palette_color),
            }),
        }),
        ..Default::default()
    };
    let merged_client_config = merge_client(ClientConfig::default(), vec![layer]);

    assert_eq!(
        merged_client_config.theme.colors,
        ColorPalette {
            ramp_start: palette_color,
            ramp_end: palette_color,
            on_ramp: palette_color,
            on_ramp_dim: palette_color,
            accent: palette_color,
            on_accent: palette_color,
            border_focused: palette_color,
            border_unfocused: palette_color,
            border_hover: palette_color,
            stack_header_fg: palette_color,
            stack_header_bg: palette_color,
            letterbox: palette_color,
            bar_bg: palette_color,
        }
    );
}

#[test]
fn a_higher_precedence_layer_can_clear_the_listen_address() {
    let user_layer = PartialKoshiConfig {
        remote_listen: Some(Some("127.0.0.1:7654".to_string())),
        ..Default::default()
    };
    let session_layer = PartialKoshiConfig {
        remote_listen: Some(None),
        ..Default::default()
    };

    let merged_server_config =
        merge_server(ServerConfig::default(), vec![user_layer, session_layer]);
    assert_eq!(merged_server_config.remote_listen, None);
}

#[test]
fn a_higher_precedence_layer_can_clear_the_shared_sessions_directory() {
    let user_layer = PartialKoshiConfig {
        shared_sessions_directory: Some(Some(PathBuf::from("/var/run/koshi"))),
        ..Default::default()
    };
    let session_layer = PartialKoshiConfig {
        shared_sessions_directory: Some(None),
        ..Default::default()
    };

    let merged_server_config =
        merge_server(ServerConfig::default(), vec![user_layer, session_layer]);
    assert_eq!(merged_server_config.shared_sessions_directory, None);
}

#[test]
fn a_higher_precedence_layer_can_clear_the_default_shell() {
    let user_layer = PartialKoshiConfig {
        terminal: Some(PartialTerminalConfig {
            term: None,
            colorterm: None,
            default_shell: Some(Some("/bin/zsh".to_string())),
            extended_keys_mode: None,
        }),
        ..Default::default()
    };
    let session_layer = PartialKoshiConfig {
        terminal: Some(PartialTerminalConfig {
            term: None,
            colorterm: None,
            default_shell: Some(None),
            extended_keys_mode: None,
        }),
        ..Default::default()
    };

    let merged_server_config =
        merge_server(ServerConfig::default(), vec![user_layer, session_layer]);
    assert_eq!(merged_server_config.terminal.default_shell, None);
    assert_eq!(merged_server_config.terminal.term, "xterm-256color"); // sibling untouched
}
