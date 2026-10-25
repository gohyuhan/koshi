//! Tests for local config commands.

use std::fs;
use std::net::SocketAddr;

use koshi_config::app_config::{parse_app_config, AppConfigFile};
use koshi_config::error::ConfigError;
use koshi_config::key::Leader;
use koshi_config::keybinding::parse_keybindings;
use koshi_config::layer::{
    PartialColorPalette, PartialCopyConfig, PartialKeybindingsConfig, PartialKoshiConfig,
    PartialLayoutDefaults, PartialLoggingConfig, PartialMouseConfig,
    PartialPaneClipboardWriteConfig, PartialPaneConfig, PartialScrollbackConfig,
    PartialTerminalConfig, PartialThemeConfig, PartialUpdateConfig,
};
use koshi_config::parser::parse_kdl;
use koshi_config::profile::parse_profile;
use koshi_config::theme::parse_theme;
use koshi_config::types::{RgbColor, WheelScroll};
use koshi_core::geometry::Direction;
use koshi_core::key::{BindingModifierFlags, ExtendedKeysMode, Key, KeyChord};
use koshi_core::log::{LogFormat, LogLevel};
use koshi_layout::template::ProfileTemplate;
use koshi_link::config::{parse_app_config_text, ProfileOutcome};
use tempfile::TempDir;

use super::*;

/// What `koshi config check` gives for `config_directory`, with `koshi.kdl`
/// read once first, as `koshi` reads it at start.
fn check_config_directory_read_at_start(config_directory: &Path) -> Result<String, CliError> {
    check_config_directory(
        config_directory,
        &load_app_config_file(Some(config_directory)).app_config_outcome,
    )
}

#[test]
fn path_prints_the_given_config_directory() {
    let config_directory = TempDir::new().unwrap();

    let config_command_output = run_config_command_in_directory(
        &ConfigCommand::Path,
        config_directory.path(),
        &AppConfigOutcome::Absent,
        None,
    )
    .unwrap();

    assert_eq!(
        config_command_output,
        format!("{}\n", config_directory.path().display())
    );
}

#[test]
fn explain_reports_file_default_and_meaning_for_each_config_file_kind() {
    let expected_explanation_by_config_key = [
        (
            "koshi.pane.min-cols",
            "koshi.pane.min-cols\nfile: koshi.kdl\ndefault: 2\nSmallest pane width in columns.\n",
        ),
        (
            "koshi.pane.gap",
            "koshi.pane.gap\nfile: koshi.kdl\ndefault: 0\n\
             Blank cells between two panes that meet along a split.\n",
        ),
        (
            "koshi.allow-beta-features",
            "koshi.allow-beta-features\nfile: koshi.kdl\ndefault: #false\n\
             Run features still marked beta.\n",
        ),
        (
            "koshi.allow-other-users",
            "koshi.allow-other-users\nfile: koshi.kdl\ndefault: #false\n\
             Let other users of this machine reach your sessions. macOS and Linux only; no effect \
             on Windows.\n",
        ),
        (
            "koshi.shared-sessions-dir",
            "koshi.shared-sessions-dir\nfile: koshi.kdl\n\
             default: \"/tmp/koshi\"\n\
             Directory the shared session sockets live in. No effect on Windows.\n",
        ),
        (
            "koshi.auto-close-session",
            "koshi.auto-close-session\nfile: koshi.kdl\ndefault: #false\n\
             End the session when its last client leaves.\n",
        ),
        (
            "koshi.remote-link-timeout-ms",
            "koshi.remote-link-timeout-ms\nfile: koshi.kdl\ndefault: 30000\n\
             Milliseconds a link to a session on another machine may carry no byte before it \
             drops, from 1000.\n",
        ),
        (
            "koshi.max-frames-per-second",
            "koshi.max-frames-per-second\nfile: koshi.kdl\ndefault: 60\n\
             Most frames a second the session sends this terminal, from 30 to 120.\n",
        ),
        (
            "koshi.terminal.default-shell",
            "koshi.terminal.default-shell\nfile: koshi.kdl\n\
             default: $SHELL (%COMSPEC% on Windows)\n\
             Shell used for a new terminal pane.\n",
        ),
        (
            "koshi.terminal.extended-keys",
            "koshi.terminal.extended-keys\nfile: koshi.kdl\ndefault: \"on-request\"\n\
             What a pane program receives for a key whose legacy bytes another key also sends: \
             \"on-request\" or \"always\".\n",
        ),
        (
            "theme.colors.border-hover",
            "theme.colors.border-hover\nfile: themes/<name>.kdl\ndefault: \"#af5fff\"\n\
             Pane border under the pointer.\n",
        ),
        (
            "keybinding.chord-timeout-ms",
            "keybinding.chord-timeout-ms\nfile: keybinding.kdl\ndefault: 500\n\
             Wait for the next key of a shortcut that is complete and also starts a longer one.\n",
        ),
        (
            "profile.version",
            "profile.version\nfile: profile/<name>.kdl\ndefault: 2\n\
             Config schema version.\n",
        ),
        (
            "profile.lock",
            "profile.lock\nfile: profile/<name>.kdl\ndefault: unset\n\
             Bare marker. Start the session's first terminal with input locked.\n",
        ),
    ];

    for (config_key, expected_explanation) in expected_explanation_by_config_key {
        assert_eq!(
            explain_config_key(config_key).unwrap(),
            expected_explanation,
            "explanation of {config_key}"
        );
    }
}

#[test]
fn explain_unknown_key_suggests_the_nearest_key() {
    let config_error = explain_config_key("koshi.pane.min-col").unwrap_err();

    assert_eq!(
        config_error.to_string(),
        "config failed: unknown key `koshi.pane.min-col`; did you mean `koshi.pane.min-cols`?"
    );
}

/// The file-qualified keys that `config_source_text` sets, sorted. With
/// `file_key_prefix` `koshi`, the top-level `theme "midnight"` gives
/// `koshi.theme`, and `min-cols 3` inside `pane { … }` gives
/// `koshi.pane.min-cols`. A top-level node named in `layout_node_names` gives
/// no key, and its children give none.
fn list_config_keys_in_text(
    file_key_prefix: &str,
    config_source_text: &str,
    layout_node_names: &[&str],
) -> Vec<String> {
    let config_document = parse_kdl(Path::new("config.kdl"), config_source_text).unwrap();
    let mut config_keys = Vec::new();
    for config_node in config_document.nodes() {
        let config_node_name = config_node.name().value();
        if layout_node_names.contains(&config_node_name) {
            continue;
        }
        match config_node.children() {
            Some(section_block) => {
                for field_node in section_block.nodes() {
                    config_keys.push(format!(
                        "{file_key_prefix}.{config_node_name}.{}",
                        field_node.name().value()
                    ));
                }
            }
            None => config_keys.push(format!("{file_key_prefix}.{config_node_name}")),
        }
    }
    config_keys.sort();
    config_keys
}

/// The keys of every [`CONFIG_FIELD_HELP`] row whose file is
/// `config_file_name`, sorted.
fn list_explained_config_keys(config_file_name: &str) -> Vec<String> {
    let mut config_keys: Vec<String> = CONFIG_FIELD_HELP
        .iter()
        .filter(|config_field_help| config_field_help.config_file_name == config_file_name)
        .map(|config_field_help| config_field_help.config_key.to_string())
        .collect();
    config_keys.sort();
    config_keys
}

#[test]
fn explain_covers_every_key_that_koshi_kdl_accepts() {
    let every_app_config_key_text = r##"version 2
theme "midnight"
allow-beta-features #true
allow-other-users #true
shared-sessions-dir "/var/run/koshi"
remote-listen "192.0.2.20:7654"
auto-close-session #true
image-support #false
reduced-motion #true
stay-in-pane-placement-mode-after-placement #false
remote-reconnect #false
remote-link-timeout-ms 45000
max-frames-per-second 120
pane {
    min-cols 3
    min-rows 2
    gap 1
}
scrollback {
    max-lines 500
    max-bytes 4096
    scroll-on-input #false
}
layout {
    new-pane-direction "down"
}
mouse {
    border-resize #false
    scroll-lines 5
    wheel "ignore"
}
copy {
    trim-trailing-whitespace #false
}
pane-clipboard-writes {
    allow #true
    max-copy-bytes 1024
}
terminal {
    term "xterm-kitty"
    colorterm "24bit"
    default-shell "/bin/sh"
    extended-keys "always"
}
logging {
    enabled #true
    level "info"
    format "json"
    max-log-file-size-in-mb 16
}
update {
    auto-check #false
    check-interval-days 7
    allow-prerelease #true
}
"##;

    let app_config_file =
        parse_app_config(Path::new("koshi.kdl"), every_app_config_key_text).unwrap();

    assert_eq!(
        app_config_file,
        AppConfigFile {
            layer: PartialKoshiConfig {
                pane: Some(PartialPaneConfig {
                    minimum_column_count: Some(3),
                    minimum_row_count: Some(2),
                    gap_cell_count: Some(1),
                }),
                scrollback: Some(PartialScrollbackConfig {
                    maximum_line_count: Some(500),
                    maximum_byte_count: Some(4096),
                    should_scroll_to_input: Some(false),
                }),
                keybindings: None,
                layout: Some(PartialLayoutDefaults {
                    new_pane_direction: Some(Direction::Down),
                }),
                mouse: Some(PartialMouseConfig {
                    can_resize_pane_border: Some(false),
                    scroll_line_count: Some(5),
                    wheel_scroll: Some(WheelScroll::Ignore),
                }),
                copy: Some(PartialCopyConfig {
                    should_trim_trailing_whitespace: Some(false),
                }),
                terminal: Some(PartialTerminalConfig {
                    term: Some("xterm-kitty".to_string()),
                    colorterm: Some("24bit".to_string()),
                    default_shell: Some(Some("/bin/sh".to_string())),
                    extended_keys_mode: Some(ExtendedKeysMode::Always),
                }),
                theme: None,
                logging: Some(PartialLoggingConfig {
                    is_enabled: Some(true),
                    log_level: Some(LogLevel::Info),
                    log_format: Some(LogFormat::Json),
                    max_log_file_size_in_mb: Some(16),
                }),
                update: Some(PartialUpdateConfig {
                    should_auto_check_for_updates: Some(false),
                    check_interval_days: Some(7),
                    should_allow_prerelease_updates: Some(true),
                }),
                supports_image_protocols: Some(false),
                should_reduce_motion: Some(true),
                should_stay_in_pane_placement_mode_after_placement: Some(false),
                should_reconnect_remote_session: Some(false),
                remote_link_timeout_ms: Some(45000),
                max_frames_per_second: Some(120),
                should_allow_beta_features: Some(true),
                should_allow_other_users: Some(true),
                remote_listen_address: Some(Some(SocketAddr::from(([192, 0, 2, 20], 7654)))),
                shared_sessions_directory: Some(Some(PathBuf::from("/var/run/koshi"))),
                should_auto_close_session: Some(true),
                pane_clipboard_writes: Some(PartialPaneClipboardWriteConfig {
                    should_allow_writes: Some(true),
                    maximum_copy_byte_count: Some(1024),
                }),
            },
            theme_name: Some("midnight".to_string()),
            parse_warnings: Vec::new(),
            schema_version: 2,
        }
    );
    assert_eq!(
        list_config_keys_in_text("koshi", every_app_config_key_text, &[]),
        list_explained_config_keys("koshi.kdl")
    );
}

#[test]
fn explain_covers_every_key_that_keybinding_kdl_accepts() {
    let every_keybinding_key_text = r#"version 2
chord-timeout-ms 750
which-key-delay-ms 100
max-chord-depth 6
leader "A-"
unlock-alternative "<C-g>"
"#;

    let keybinding_layer =
        parse_keybindings(Path::new("keybinding.kdl"), every_keybinding_key_text).unwrap();

    assert_eq!(
        keybinding_layer,
        PartialKeybindingsConfig {
            chord_timeout_ms: Some(750),
            which_key_delay_ms: Some(100),
            maximum_chord_depth: Some(6),
            leader: Some(Leader::Modifiers(BindingModifierFlags::ALT)),
            mode_bindings_by_name: None,
            unlock_alternative: Some(Some(KeyChord::from_parts(
                BindingModifierFlags::CTRL,
                Key::Char('g'),
            ))),
        }
    );
    assert_eq!(
        list_config_keys_in_text("keybinding", every_keybinding_key_text, &[]),
        list_explained_config_keys("keybinding.kdl")
    );
}

#[test]
fn explain_covers_every_key_that_a_theme_file_accepts() {
    let every_theme_key_text = r##"version 2
colors {
    ramp-start "#000001"
    ramp-end "#000002"
    on-ramp "#000003"
    on-ramp-dim "#000004"
    accent "#000005"
    on-accent "#000006"
    bar-bg "#000007"
    border-focused "#000008"
    border-unfocused "#000009"
    border-hover "#00000a"
    stack-header-fg "#00000b"
    stack-header-bg "#00000c"
    letterbox "#00000d"
}
"##;

    let theme_layer = parse_theme(Path::new("themes/test.kdl"), every_theme_key_text).unwrap();

    assert_eq!(
        theme_layer,
        (
            PartialThemeConfig {
                theme_name: None,
                colors: Some(PartialColorPalette {
                    ramp_start: Some(RgbColor::from_channels(0, 0, 1)),
                    ramp_end: Some(RgbColor::from_channels(0, 0, 2)),
                    on_ramp: Some(RgbColor::from_channels(0, 0, 3)),
                    on_ramp_dim: Some(RgbColor::from_channels(0, 0, 4)),
                    accent: Some(RgbColor::from_channels(0, 0, 5)),
                    on_accent: Some(RgbColor::from_channels(0, 0, 6)),
                    bar_bg: Some(RgbColor::from_channels(0, 0, 7)),
                    border_focused: Some(RgbColor::from_channels(0, 0, 8)),
                    border_unfocused: Some(RgbColor::from_channels(0, 0, 9)),
                    border_hover: Some(RgbColor::from_channels(0, 0, 10)),
                    stack_header_fg: Some(RgbColor::from_channels(0, 0, 11)),
                    stack_header_bg: Some(RgbColor::from_channels(0, 0, 12)),
                    letterbox: Some(RgbColor::from_channels(0, 0, 13)),
                }),
            },
            Vec::new(),
        )
    );
    assert_eq!(
        list_config_keys_in_text("theme", every_theme_key_text, &[]),
        list_explained_config_keys("themes/<name>.kdl")
    );
}

#[test]
fn explain_covers_every_key_that_a_profile_file_accepts() {
    let every_profile_key_text = "version 2\nlock\ntab {\n    pane\n}\n";

    let ProfileTemplate {
        tabs: _,
        focused_tab_index: _,
        is_locked,
    } = parse_profile(Path::new("profile/test.kdl"), every_profile_key_text).unwrap();

    assert!(is_locked);
    assert_eq!(
        list_config_keys_in_text("profile", every_profile_key_text, &["tab"]),
        list_explained_config_keys("profile/<name>.kdl")
    );
}

#[test]
fn check_validates_every_known_file_in_sorted_path_order() {
    let config_directory = TempDir::new().unwrap();
    fs::create_dir(config_directory.path().join("themes")).unwrap();
    fs::create_dir(config_directory.path().join("profile")).unwrap();
    fs::write(config_directory.path().join("koshi.kdl"), "version 2\n").unwrap();
    fs::write(
        config_directory.path().join("keybinding.kdl"),
        "version 2\nmode \"normal\" {}\n",
    )
    .unwrap();
    fs::write(
        config_directory.path().join("themes").join("z.kdl"),
        "version 2\ncolors {}\n",
    )
    .unwrap();
    fs::write(
        config_directory.path().join("profile").join("a.kdl"),
        "version 2\ntab { pane }\n",
    )
    .unwrap();
    fs::write(
        config_directory.path().join("themes").join("skip.txt"),
        "not config",
    )
    .unwrap();

    let report_text = check_config_directory_read_at_start(config_directory.path()).unwrap();

    assert_eq!(
        report_text,
        format!(
            "{}: valid (version 2)\n{}: valid (version 2)\n{}: valid (version 2)\n{}: valid (version 2)\n",
            config_directory.path().join("keybinding.kdl").display(),
            config_directory.path().join("koshi.kdl").display(),
            config_directory.path().join("profile").join("a.kdl").display(),
            config_directory.path().join("themes").join("z.kdl").display(),
        )
    );
}

#[test]
fn check_collects_errors_from_all_files() {
    let config_directory = TempDir::new().unwrap();
    fs::create_dir(config_directory.path().join("themes")).unwrap();
    fs::write(config_directory.path().join("koshi.kdl"), "pane {}\n").unwrap();
    fs::write(
        config_directory.path().join("themes").join("bad.kdl"),
        "version 2\ncolors { accent \"bad\" }\n",
    )
    .unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path())
        .unwrap_err()
        .to_string();

    assert_eq!(
        config_error,
        format!(
            "config failed: invalid config file {}: line 1, column 1: file must declare `version`\ninvalid config file {}: line 2, column 10: ignored `colors.accent`: color must be 6 hex digits (#RRGGBB), got 3",
            config_directory.path().join("koshi.kdl").display(),
            config_directory.path().join("themes").join("bad.kdl").display(),
        )
    );
}

#[test]
fn check_lists_every_problem_of_a_refused_app_file_in_file_order() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(
        &app_config_path,
        "version 2\nfrobnicate #true\npane {\n}\nupdate { auto-check \"yes\" }\n",
    )
    .unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: invalid config file {}: line 2, column 1: ignored unknown key \
             `frobnicate`; did you mean `update`?; line 5, column 10: invalid config key \
             `update.auto-check`: expected a boolean (#true or #false)",
            app_config_path.display()
        )
    );
}

#[test]
fn check_lists_a_missing_version_with_every_other_problem_in_file_order() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(
        &app_config_path,
        "frobnicate 1\nupdate { auto-check \"yes\" }\n",
    )
    .unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: invalid config file {}: line 1, column 1: ignored unknown key \
             `frobnicate`; did you mean `update`?; line 1, column 1: file must declare `version`; \
             line 2, column 10: invalid config key `update.auto-check`: expected a boolean \
             (#true or #false)",
            app_config_path.display()
        )
    );
}

#[test]
fn check_names_only_the_version_of_a_file_from_a_newer_koshi() {
    let newer_schema_version = koshi_config::types::SCHEMA_VERSION + 1;
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(
        &app_config_path,
        format!("version {newer_schema_version}\nfrobnicate 1\n"),
    )
    .unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: invalid config file {}: line 1, column 1: config schema version \
             {newer_schema_version} is newer than this koshi supports ({})",
            app_config_path.display(),
            koshi_config::types::SCHEMA_VERSION
        )
    );
}

#[test]
fn check_lists_every_bad_update_field() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(
        &app_config_path,
        "version 2\nupdate {\n    auto-check \"yes\"\n    check-interval-days -3\n}\n",
    )
    .unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: invalid config file {}: line 3, column 5: invalid config key \
             `update.auto-check`: expected a boolean (#true or #false); line 4, column 5: \
             invalid config key `update.check-interval-days`: must be between 0 and 4294967295",
            app_config_path.display()
        )
    );
}

#[test]
fn check_names_the_line_and_column_of_each_file_that_fails_as_a_whole() {
    let config_directory = TempDir::new().unwrap();
    let keybinding_config_path = config_directory.path().join("keybinding.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    let theme_config_path = config_directory.path().join("themes").join("repeated.kdl");
    fs::create_dir(config_directory.path().join("themes")).unwrap();
    fs::write(&keybinding_config_path, "version 0\n").unwrap();
    fs::write(
        &app_config_path,
        "version 2\nupdate {\n    auto-check \"yes\"\n}\n",
    )
    .unwrap();
    fs::write(&theme_config_path, "version 2\n  version 2\n").unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: invalid config file {}: line 1, column 1: config schema version must \
             be at least 1\ninvalid config file {}: line 3, column 5: invalid config key \
             `update.auto-check`: expected a boolean (#true or #false)\ninvalid config file {}: \
             line 2, column 3: `version` is declared more than once",
            keybinding_config_path.display(),
            app_config_path.display(),
            theme_config_path.display(),
        )
    );
}

/// Runs `koshi config check` on a new config directory whose only files are
/// the profiles in `profile_texts_by_file_name`. Returns the directory and the
/// check's error text.
fn run_config_check_on_profiles(profile_texts_by_file_name: &[(&str, &str)]) -> (TempDir, String) {
    let config_directory = TempDir::new().unwrap();
    let profile_directory = config_directory.path().join("profile");
    fs::create_dir(&profile_directory).unwrap();
    for (profile_file_name, profile_text) in profile_texts_by_file_name {
        fs::write(profile_directory.join(profile_file_name), profile_text).unwrap();
    }
    let config_error = check_config_directory_read_at_start(config_directory.path())
        .unwrap_err()
        .to_string();
    (config_directory, config_error)
}

#[test]
fn check_prints_one_profile_problem_exactly_once() {
    let (config_directory, config_error) =
        run_config_check_on_profiles(&[("a.kdl", "version 2\ntab { pane { size 30 } }\n")]);

    let profile_directory = config_directory.path().join("profile");
    assert_eq!(
        config_error,
        format!(
            "config failed: invalid config file {}: line 2, column 14: sizing applies only to \
             children of `horizontal` or `vertical`",
            profile_directory.join("a.kdl").display(),
        )
    );
}

#[test]
fn check_names_the_location_of_one_profile_problem_repeated_on_two_nodes() {
    let (config_directory, config_error) =
        run_config_check_on_profiles(&[("a.kdl", "version 2\ntab { pane { size 30; min 5 } }\n")]);

    let profile_directory = config_directory.path().join("profile");
    assert_eq!(
        config_error,
        format!(
            "config failed: invalid config file {}: line 2, column 14: sizing applies only to \
             children of `horizontal` or `vertical`; line 2, column 23: sizing applies only to \
             children of `horizontal` or `vertical`",
            profile_directory.join("a.kdl").display(),
        )
    );
}

#[test]
fn check_prints_two_different_profile_problems_once_each_in_file_order() {
    let (config_directory, config_error) = run_config_check_on_profiles(&[(
        "a.kdl",
        "version 2\ntab { horizontal { size 30; pane \"x\"; pane } }\n",
    )]);

    let profile_directory = config_directory.path().join("profile");
    assert_eq!(
        config_error,
        format!(
            "config failed: invalid config file {}: line 2, column 20: sizing applies only to \
             children of `horizontal` or `vertical`; line 2, column 29: `pane` takes no \
             arguments or properties; its configuration goes in the children block",
            profile_directory.join("a.kdl").display(),
        )
    );
}

#[test]
fn check_prints_the_same_profile_problem_once_per_file() {
    let (config_directory, config_error) = run_config_check_on_profiles(&[
        ("a.kdl", "version 2\ntab { pane { size 30 } }\n"),
        ("b.kdl", "version 2\ntab { pane { size 30 } }\n"),
    ]);

    let profile_directory = config_directory.path().join("profile");
    assert_eq!(
        config_error,
        format!(
            "config failed: invalid config file {}: line 2, column 14: sizing applies only to \
             children of `horizontal` or `vertical`\ninvalid config file {}: line 2, column 14: \
             sizing applies only to children of `horizontal` or `vertical`",
            profile_directory.join("a.kdl").display(),
            profile_directory.join("b.kdl").display(),
        )
    );
}

#[test]
fn check_names_the_location_of_the_same_app_config_warning_on_two_nodes() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(
        &app_config_path,
        "version 2\npane {\n    bogus 1\n    bogus 2\n}\npane {}\n",
    )
    .unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path())
        .unwrap_err()
        .to_string();

    assert_eq!(
        config_error,
        format!(
            "config failed: invalid config file {}: line 3, column 5: ignored unknown key \
             `pane.bogus`; did you mean `pane.gap`?; line 4, column 5: ignored unknown key \
             `pane.bogus`; did you mean `pane.gap`?; line 6, column 1: ignored duplicate `pane` \
             section",
            app_config_path.display(),
        )
    );
}

#[test]
fn check_rejects_a_config_path_that_is_not_a_regular_file() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::create_dir(&app_config_path).unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: expected a regular file",
            app_config_path.display()
        )
    );
}

#[test]
fn check_rejects_a_kdl_directory_below_a_config_folder() {
    let config_directory = TempDir::new().unwrap();
    let theme_config_path = config_directory.path().join("themes").join("bad.kdl");
    fs::create_dir_all(&theme_config_path).unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: expected a regular file",
            theme_config_path.display()
        )
    );
}

#[test]
fn check_reports_read_and_validation_errors_together() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let theme_config_path = config_directory.path().join("themes").join("bad.kdl");
    fs::create_dir(&app_config_path).unwrap();
    fs::create_dir_all(theme_config_path.parent().unwrap()).unwrap();
    fs::write(&theme_config_path, "colors {}\n").unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: expected a regular file\ninvalid config file {}: line 1, column 1: file must declare `version`",
            app_config_path.display(),
            theme_config_path.display()
        )
    );
}

#[test]
fn migrate_command_updates_a_schema_one_file_to_schema_two() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();

    let migration_report = run_config_command_in_directory(
        &ConfigCommand::Migrate,
        config_directory.path(),
        &AppConfigOutcome::Absent,
        None,
    )
    .unwrap();

    assert_eq!(
        migration_report,
        format!("{}: migrated version 1 to 2\n", app_config_path.display())
    );
    assert_eq!(fs::read_to_string(app_config_path).unwrap(), "version 2\n");
}

#[test]
fn migrate_command_keeps_an_unknown_key_in_a_migrated_file() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\nmade-up-key \"x\"\n").unwrap();

    let migration_report = run_config_command_in_directory(
        &ConfigCommand::Migrate,
        config_directory.path(),
        &AppConfigOutcome::Absent,
        None,
    )
    .unwrap();

    assert_eq!(
        migration_report,
        format!("{}: migrated version 1 to 2\n", app_config_path.display())
    );
    assert_eq!(
        fs::read_to_string(app_config_path).unwrap(),
        "version 2\nmade-up-key \"x\"\n"
    );
}

/// Rejects any `bad.kdl` with `bad source`, and migrates every other file from
/// version 1 to 2 by appending `migrated #true`.
fn migrate_every_file_except_bad_kdl(
    _config_file_kind: ConfigFileKind,
    source_config_file: &ParsedConfigFile,
) -> Result<MigratedConfig, MigrationError> {
    let config_path = source_config_file.get_config_path();
    if config_path.ends_with("bad.kdl") {
        return Err(MigrationError::Invalid {
            config_path: config_path.display().to_string(),
            validation_error_detail: "bad source".to_string(),
        });
    }
    Ok(MigratedConfig {
        source_schema_version: 1,
        target_schema_version: 2,
        migrated_config_file: Some(ParsedConfigFile::parse(
            config_path,
            source_config_file.get_source_text().to_string() + "migrated #true\n",
        )),
    })
}

#[test]
fn migrate_writes_nothing_when_any_source_is_invalid() {
    let config_directory = TempDir::new().unwrap();
    fs::create_dir(config_directory.path().join("themes")).unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let invalid_theme_path = config_directory.path().join("themes").join("bad.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();
    fs::write(&invalid_theme_path, "version 1\n").unwrap();

    let config_error = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_every_file_except_bad_kdl,
        write_atomic,
    )
    .config_report_text
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: migration stopped before writing any file:\ninvalid config file {}: bad source",
            invalid_theme_path.display()
        )
    );
    assert_eq!(fs::read_to_string(app_config_path).unwrap(), "version 1\n");
    assert_eq!(
        fs::read_to_string(invalid_theme_path).unwrap(),
        "version 1\n"
    );
}

#[test]
fn migrate_replaces_each_changed_file_after_validation() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_every_file_except_bad_kdl,
        write_atomic,
    );

    assert_eq!(
        migration_report.config_report_text.unwrap(),
        format!("{}: migrated version 1 to 2\n", app_config_path.display())
    );
    assert_eq!(
        migration_report.config_files,
        vec![ParsedConfigFile::parse(
            &app_config_path,
            "version 1\nmigrated #true\n".to_string()
        )]
    );
    assert_eq!(
        fs::read_to_string(app_config_path).unwrap(),
        "version 1\nmigrated #true\n"
    );
}

#[cfg(unix)]
#[test]
fn migrate_updates_a_symlink_target_and_keeps_the_link() {
    use std::os::unix::fs::symlink;

    let config_directory = TempDir::new().unwrap();
    let stored_config_path = config_directory.path().join("stored-koshi.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&stored_config_path, "version 1\n").unwrap();
    symlink(&stored_config_path, &app_config_path).unwrap();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_every_file_except_bad_kdl,
        write_atomic,
    );

    assert_eq!(
        migration_report.config_report_text.unwrap(),
        format!("{}: migrated version 1 to 2\n", app_config_path.display())
    );
    assert!(fs::symlink_metadata(&app_config_path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_to_string(stored_config_path).unwrap(),
        "version 1\nmigrated #true\n"
    );
}

#[test]
fn migrate_write_failure_reports_files_already_migrated() {
    let config_directory = TempDir::new().unwrap();
    let keybinding_config_path = config_directory.path().join("keybinding.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&keybinding_config_path, "version 1\n").unwrap();
    fs::write(&app_config_path, "version 1\n").unwrap();
    let mut write_attempt_count = 0;

    let config_error = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_every_file_except_bad_kdl,
        |config_path, serialized_config_bytes| {
            write_attempt_count += 1;
            if write_attempt_count == 2 {
                return Err(StorageError::Io {
                    detail: "injected failure".to_string(),
                });
            }
            write_atomic(config_path, serialized_config_bytes)
        },
    )
    .config_report_text
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: migration write failed for {}: storage io error: injected failure\n{} may already contain migrated data; check it before retrying\nfiles already migrated before this failure:\n{}: migrated version 1 to 2",
            app_config_path.display(),
            app_config_path.display(),
            keybinding_config_path.display(),
        )
    );
    assert_eq!(
        fs::read_to_string(keybinding_config_path).unwrap(),
        "version 1\nmigrated #true\n"
    );
    assert_eq!(fs::read_to_string(app_config_path).unwrap(), "version 1\n");
}

#[test]
fn migrate_write_failure_warns_that_the_failing_file_may_have_changed() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();

    let config_error = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_every_file_except_bad_kdl,
        |config_path, serialized_config_bytes| {
            write_atomic(config_path, serialized_config_bytes)?;
            Err(StorageError::Io {
                detail: "injected fsync failure".to_string(),
            })
        },
    )
    .config_report_text
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: migration write failed for {}: storage io error: injected fsync failure\n{} may already contain migrated data; check it before retrying",
            app_config_path.display(),
            app_config_path.display(),
        )
    );
    assert_eq!(
        fs::read_to_string(app_config_path).unwrap(),
        "version 1\nmigrated #true\n"
    );
}

#[test]
fn check_of_a_directory_holding_no_config_file_says_so_and_names_the_directory() {
    let config_directory = TempDir::new().unwrap();

    let report_text = check_config_directory_read_at_start(config_directory.path()).unwrap();

    assert_eq!(
        report_text,
        format!(
            "no config files found in {}\n",
            config_directory.path().display()
        )
    );
}

#[test]
fn migrate_of_a_directory_holding_no_config_file_says_so_and_writes_nothing() {
    let config_directory = TempDir::new().unwrap();
    let mut written_config_paths: Vec<PathBuf> = Vec::new();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_every_file_except_bad_kdl,
        |config_path, _serialized_config_bytes| {
            written_config_paths.push(config_path.to_path_buf());
            Ok(())
        },
    );

    assert_eq!(
        migration_report.config_report_text.unwrap(),
        format!(
            "no config files found in {}\n",
            config_directory.path().display()
        )
    );
    assert_eq!(written_config_paths, Vec::<PathBuf>::new());
}

#[test]
fn run_config_command_in_directory_routes_explain_to_the_field_table() {
    let config_directory = TempDir::new().unwrap();

    let config_command_output = run_config_command_in_directory(
        &ConfigCommand::Explain {
            config_key: "koshi.pane.gap".to_string(),
        },
        config_directory.path(),
        &AppConfigOutcome::Absent,
        None,
    )
    .unwrap();

    assert_eq!(
        config_command_output,
        "koshi.pane.gap\nfile: koshi.kdl\ndefault: 0\n\
         Blank cells between two panes that meet along a split.\n"
    );
}

#[test]
fn run_config_command_in_directory_routes_check_to_the_directory_scan() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 2\n").unwrap();

    let config_command_output = run_config_command_in_directory(
        &ConfigCommand::Check,
        config_directory.path(),
        &load_app_config_file(Some(config_directory.path())).app_config_outcome,
        None,
    )
    .unwrap();

    assert_eq!(
        config_command_output,
        format!("{}: valid (version 2)\n", app_config_path.display())
    );
}

/// `migrate` runs the real migration table. A `version 2` file is reported as
/// current and left byte for byte as it is.
#[test]
fn run_config_command_in_directory_migrate_leaves_a_file_already_on_this_schema_untouched() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 2\n").unwrap();

    let config_command_output = run_config_command_in_directory(
        &ConfigCommand::Migrate,
        config_directory.path(),
        &AppConfigOutcome::Absent,
        None,
    )
    .unwrap();

    assert_eq!(
        config_command_output,
        format!("{}: current (version 2)\n", app_config_path.display())
    );
    assert_eq!(fs::read_to_string(&app_config_path).unwrap(), "version 2\n");
}

#[test]
fn check_reports_a_themes_path_that_is_not_a_directory() {
    let config_directory = TempDir::new().unwrap();
    let themes_path = config_directory.path().join("themes");
    fs::write(&themes_path, "not a directory").unwrap();
    let io_error = fs::read_dir(&themes_path).unwrap_err();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!("config failed: read {}: {io_error}", themes_path.display())
    );
}

#[cfg(unix)]
#[test]
fn check_validates_a_config_symlink_through_its_target() {
    use std::os::unix::fs::symlink;

    let config_directory = TempDir::new().unwrap();
    let stored_config_path = config_directory.path().join("stored-koshi.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&stored_config_path, "version 2\n").unwrap();
    symlink(&stored_config_path, &app_config_path).unwrap();

    let report_text = check_config_directory_read_at_start(config_directory.path()).unwrap();

    assert_eq!(
        report_text,
        format!("{}: valid (version 2)\n", app_config_path.display())
    );
}

#[cfg(unix)]
#[test]
fn check_reports_a_config_symlink_that_points_nowhere() {
    use std::os::unix::fs::symlink;

    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    symlink(
        config_directory.path().join("missing.kdl"),
        &app_config_path,
    )
    .unwrap();
    let io_error = fs::canonicalize(&app_config_path).unwrap_err();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: {io_error}",
            app_config_path.display()
        )
    );
}

#[cfg(unix)]
#[test]
fn check_rejects_a_config_symlink_that_points_at_a_directory() {
    use std::os::unix::fs::symlink;

    let config_directory = TempDir::new().unwrap();
    let target_directory = config_directory.path().join("target-dir");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::create_dir(&target_directory).unwrap();
    symlink(&target_directory, &app_config_path).unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: expected a regular file",
            app_config_path.display()
        )
    );
}

#[test]
fn check_validates_the_koshi_kdl_text_read_at_start_and_not_the_text_on_disk() {
    // `koshi.kdl` on disk names no `version`; the text read at start is
    // `version 2`.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "pane {}\n").unwrap();
    let app_config_outcome =
        parse_app_config_text(&app_config_path, "version 2\n".to_string()).app_config_outcome;

    let report_text = check_config_directory(config_directory.path(), &app_config_outcome).unwrap();

    assert_eq!(
        report_text,
        format!("{}: valid (version 2)\n", app_config_path.display())
    );
}

#[test]
fn check_reports_the_read_error_of_the_koshi_kdl_read_at_start() {
    // `koshi.kdl` on disk is valid; the read at start failed.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 2\n").unwrap();

    let config_error = check_config_directory(
        config_directory.path(),
        &AppConfigOutcome::Unreadable {
            read_error_text: "Permission denied (os error 13)".to_string(),
        },
    )
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: Permission denied (os error 13)",
            app_config_path.display()
        )
    );
}

#[test]
fn check_finds_no_koshi_kdl_when_none_was_read_at_start() {
    // `koshi.kdl` was absent at start and is on disk now.
    let config_directory = TempDir::new().unwrap();
    fs::write(config_directory.path().join("koshi.kdl"), "version 2\n").unwrap();

    let report_text =
        check_config_directory(config_directory.path(), &AppConfigOutcome::Absent).unwrap();

    assert_eq!(
        report_text,
        format!(
            "no config files found in {}\n",
            config_directory.path().display()
        )
    );
}

#[test]
fn check_rejects_a_koshi_kdl_that_is_no_regular_file_now_whatever_text_was_read_at_start() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::create_dir(&app_config_path).unwrap();
    let app_config_outcome =
        parse_app_config_text(&app_config_path, "version 2\n".to_string()).app_config_outcome;

    let config_error =
        check_config_directory(config_directory.path(), &app_config_outcome).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: expected a regular file",
            app_config_path.display()
        )
    );
}

#[test]
fn check_reports_a_koshi_kdl_that_is_not_utf8_with_its_read_error() {
    // `0x80` starts no UTF-8 sequence: the read fails on every platform.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, [0x76, 0x65, 0x80, 0x72]).unwrap();
    let read_error = fs::read_to_string(&app_config_path).unwrap_err();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: {read_error}",
            app_config_path.display()
        )
    );
}

#[test]
fn check_validates_koshi_kdl_from_what_the_start_read_found_and_parses_no_text_again() {
    // The start read found `version 1` with no problem. The text on disk is
    // not KDL.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "pane {\n").unwrap();
    let app_config_outcome = AppConfigOutcome::Parsed {
        app_config_file: Box::new(AppConfigFile {
            schema_version: 1,
            layer: PartialKoshiConfig::default(),
            theme_name: None,
            parse_warnings: Vec::new(),
        }),
    };

    let report_text = check_config_directory(config_directory.path(), &app_config_outcome).unwrap();

    assert_eq!(
        report_text,
        format!(
            "{}: valid (version 1; migrate to version 2)\n",
            app_config_path.display()
        )
    );
}

#[test]
fn check_reports_the_problems_the_start_read_found_and_parses_no_text_again() {
    // The start read refused the file for one problem. The text on disk is a
    // valid `version 2`.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 2\n").unwrap();
    let app_config_outcome = AppConfigOutcome::Refused {
        config_error: ConfigError::Validation {
            problem_messages: vec!["line 9, column 9: a problem the start read found".to_string()],
        },
    };

    let config_error =
        check_config_directory(config_directory.path(), &app_config_outcome).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: invalid config file {}: line 9, column 9: a problem the start read \
             found",
            app_config_path.display()
        )
    );
}

/// What `koshi config check` gives for a new config directory whose only file
/// is a `koshi.kdl` holding `app_config_text`, read once at start. Also gives
/// the directory and the `koshi.kdl` path. An error is its text.
fn run_config_check_on_app_config(
    app_config_text: &str,
) -> (TempDir, PathBuf, Result<String, String>) {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, app_config_text).unwrap();
    let check_result = check_config_directory_read_at_start(config_directory.path())
        .map_err(|config_error| config_error.to_string());
    (config_directory, app_config_path, check_result)
}

#[test]
fn check_names_the_line_and_column_of_a_koshi_kdl_that_is_not_kdl() {
    let (_config_directory, app_config_path, check_result) =
        run_config_check_on_app_config("version 2\npane {\n");

    assert_eq!(
        check_result,
        Err(format!(
            "config failed: config parse error in {}: line 2, column 6: No closing '}}' for \
             child block",
            app_config_path.display()
        ))
    );
}

#[test]
fn check_names_a_repeated_koshi_kdl_version() {
    let (_config_directory, app_config_path, check_result) =
        run_config_check_on_app_config("version 2\nversion 2\n");

    assert_eq!(
        check_result,
        Err(format!(
            "config failed: invalid config file {}: line 2, column 1: `version` is declared \
             more than once",
            app_config_path.display()
        ))
    );
}

#[test]
fn check_names_a_koshi_kdl_version_that_is_no_integer() {
    let (_config_directory, app_config_path, check_result) =
        run_config_check_on_app_config("version \"two\"\n");

    assert_eq!(
        check_result,
        Err(format!(
            "config failed: invalid config file {}: line 1, column 9: `version` must be an \
             integer from 1 to 4294967295",
            app_config_path.display()
        ))
    );
}

#[test]
fn check_names_a_koshi_kdl_version_of_zero() {
    let (_config_directory, app_config_path, check_result) =
        run_config_check_on_app_config("version 0\n");

    assert_eq!(
        check_result,
        Err(format!(
            "config failed: invalid config file {}: line 1, column 1: config schema version \
             must be at least 1",
            app_config_path.display()
        ))
    );
}

#[test]
fn check_reports_an_older_valid_koshi_kdl_as_valid_with_the_version_to_migrate_to() {
    let (_config_directory, app_config_path, check_result) =
        run_config_check_on_app_config("version 1\n");

    assert_eq!(
        check_result,
        Ok(format!(
            "{}: valid (version 1; migrate to version 2)\n",
            app_config_path.display()
        ))
    );
}

#[test]
fn check_reports_the_warning_of_an_older_koshi_kdl() {
    let (_config_directory, app_config_path, check_result) =
        run_config_check_on_app_config("version 1\nfrobnicate 1\n");

    assert_eq!(
        check_result,
        Err(format!(
            "config failed: invalid config file {}: line 2, column 1: ignored unknown key \
             `frobnicate`; did you mean `update`?",
            app_config_path.display()
        ))
    );
}

#[test]
fn check_lists_read_errors_before_validation_errors_whatever_their_path_order() {
    // `koshi.kdl` sorts before `profile/a.kdl`; its validation error still
    // comes after the profile's read error.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let profile_path = config_directory.path().join("profile").join("a.kdl");
    fs::write(&app_config_path, "pane {}\n").unwrap();
    fs::create_dir_all(&profile_path).unwrap();

    let config_error = check_config_directory_read_at_start(config_directory.path()).unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: read {}: expected a regular file\ninvalid config file {}: line 1, \
             column 1: file must declare `version`",
            profile_path.display(),
            app_config_path.display()
        )
    );
}

#[test]
fn migrate_writes_nothing_when_a_config_directory_cannot_be_read() {
    let config_directory = TempDir::new().unwrap();
    let themes_path = config_directory.path().join("themes");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&themes_path, "not a directory").unwrap();
    fs::write(&app_config_path, "version 1\n").unwrap();
    let io_error = fs::read_dir(&themes_path).unwrap_err();
    let mut written_config_paths: Vec<PathBuf> = Vec::new();

    let config_error = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_every_file_except_bad_kdl,
        |config_path, _serialized_config_bytes| {
            written_config_paths.push(config_path.to_path_buf());
            Ok(())
        },
    )
    .config_report_text
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: migration stopped before writing any file:\nread {}: {io_error}",
            themes_path.display()
        )
    );
    assert_eq!(written_config_paths, Vec::<PathBuf>::new());
    assert_eq!(fs::read_to_string(app_config_path).unwrap(), "version 1\n");
}

/// Migrates `koshi.kdl` from version 1 to 2 and reports every other file as
/// already on version 2.
fn migrate_only_the_app_file(
    config_file_kind: ConfigFileKind,
    source_config_file: &ParsedConfigFile,
) -> Result<MigratedConfig, MigrationError> {
    if config_file_kind == ConfigFileKind::App {
        return Ok(MigratedConfig {
            source_schema_version: 1,
            target_schema_version: 2,
            migrated_config_file: Some(ParsedConfigFile::parse(
                source_config_file.get_config_path(),
                source_config_file.get_source_text().to_string() + "migrated #true\n",
            )),
        });
    }
    Ok(MigratedConfig {
        source_schema_version: 2,
        target_schema_version: 2,
        migrated_config_file: None,
    })
}

#[test]
fn migrate_writes_only_the_files_that_changed() {
    let config_directory = TempDir::new().unwrap();
    let keybinding_config_path = config_directory.path().join("keybinding.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&keybinding_config_path, "version 1\n").unwrap();
    fs::write(&app_config_path, "version 1\n").unwrap();
    let mut written_config_paths: Vec<PathBuf> = Vec::new();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_only_the_app_file,
        |config_path, serialized_config_bytes| {
            written_config_paths.push(config_path.to_path_buf());
            write_atomic(config_path, serialized_config_bytes)
        },
    );

    assert_eq!(
        migration_report.config_report_text.unwrap(),
        format!(
            "{}: current (version 2)\n{}: migrated version 1 to 2\n",
            keybinding_config_path.display(),
            app_config_path.display()
        )
    );
    assert_eq!(written_config_paths, vec![app_config_path.clone()]);
    assert_eq!(
        fs::read_to_string(keybinding_config_path).unwrap(),
        "version 1\n"
    );
    assert_eq!(
        fs::read_to_string(app_config_path).unwrap(),
        "version 1\nmigrated #true\n"
    );
}

#[test]
fn update_migrates_every_config_kind_without_a_user_command() {
    let config_directory = TempDir::new().expect("create config directory");
    let config_files = [
        (
            config_directory.path().join("koshi.kdl"),
            "version 1\n",
            "version 2\n",
        ),
        (
            config_directory.path().join("keybinding.kdl"),
            "version 1\n",
            "version 2\n",
        ),
        (
            config_directory.path().join("themes").join("plain.kdl"),
            "version 1\n",
            "version 2\n",
        ),
        (
            config_directory.path().join("profile").join("work.kdl"),
            "version 1\ntab { pane }\n",
            "version 2\ntab { pane }\n",
        ),
    ];
    for (config_path, old_config_source_text, _) in &config_files {
        fs::create_dir_all(config_path.parent().expect("config parent"))
            .expect("create config parent");
        fs::write(config_path, old_config_source_text).expect("write old schema");
    }

    let (_, config_migration_error) = migrate_config_directory_for_update(config_directory.path());
    assert_eq!(
        config_migration_error.map(|migration_error| migration_error.to_string()),
        None
    );
    for (config_path, _, migrated_config_source_text) in &config_files {
        assert_eq!(
            &fs::read_to_string(config_path).expect("read migrated config"),
            migrated_config_source_text
        );
    }
    let (_, config_migration_error) = migrate_config_directory_for_update(config_directory.path());
    assert_eq!(
        config_migration_error.map(|migration_error| migration_error.to_string()),
        None
    );
    for (config_path, _, migrated_config_source_text) in &config_files {
        assert_eq!(
            &fs::read_to_string(config_path).expect("read current config"),
            migrated_config_source_text
        );
    }
}

#[test]
fn two_concurrent_updates_leave_the_config_on_the_new_schema() {
    let config_directory = TempDir::new().expect("create config directory");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").expect("write old schema");
    let start_barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut migration_threads = Vec::new();
    for _migration_index in 0..2 {
        let config_directory_path = config_directory.path().to_path_buf();
        let thread_barrier = std::sync::Arc::clone(&start_barrier);
        migration_threads.push(std::thread::spawn(move || {
            thread_barrier.wait();
            migrate_config_directory_for_update(&config_directory_path)
        }));
    }
    start_barrier.wait();
    for migration_thread in migration_threads {
        let (_, config_migration_error) =
            migration_thread.join().expect("migration thread completed");
        assert_eq!(
            config_migration_error.map(|migration_error| migration_error.to_string()),
            None
        );
    }
    assert_eq!(
        fs::read_to_string(app_config_path).expect("read migrated config"),
        "version 2\n"
    );
}

#[test]
fn update_without_config_directory_creates_no_config_files() {
    let test_directory = TempDir::new().expect("create test directory");
    let missing_config_directory = test_directory.path().join("missing-config");

    let (config_files, config_migration_error) =
        migrate_config_directory_for_update(&missing_config_directory);

    assert_eq!(config_files, Vec::new());
    assert_eq!(
        config_migration_error.map(|migration_error| migration_error.to_string()),
        None
    );
    assert!(!missing_config_directory.exists());
}

#[test]
fn update_refuses_config_migration_when_its_lock_cannot_open() {
    let config_directory = TempDir::new().expect("create config directory");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").expect("write released config");
    let migration_lock_path = config_directory.path().join(".migration.lock");
    fs::create_dir(&migration_lock_path).expect("block migration lock");
    let expected_open_error = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&migration_lock_path)
        .expect_err("a directory cannot be opened as a migration lock");

    let (config_files, config_migration_error) =
        migrate_config_directory_for_update(config_directory.path());

    assert_eq!(
        config_migration_error.map(|migration_error| migration_error.to_string()),
        Some(format!(
            "config failed: open {}: {expected_open_error}",
            migration_lock_path.display()
        ))
    );
    assert_eq!(
        config_files,
        vec![ParsedConfigFile::parse(
            &app_config_path,
            "version 1\n".to_string()
        )]
    );
    assert_eq!(
        fs::read_to_string(app_config_path).expect("read unchanged config"),
        "version 1\n"
    );
}

#[test]
fn update_reads_current_config_without_opening_the_migration_lock() {
    let config_directory = TempDir::new().expect("create config directory");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 2\n").expect("write current config");
    fs::create_dir(config_directory.path().join(".migration.lock")).expect("block migration lock");

    let (config_files, config_migration_error) =
        migrate_config_directory_for_update(config_directory.path());

    assert_eq!(
        config_migration_error.map(|migration_error| migration_error.to_string()),
        None
    );
    assert_eq!(
        config_files,
        vec![ParsedConfigFile::parse(
            &app_config_path,
            "version 2\n".to_string()
        )]
    );
    assert_eq!(
        fs::read_to_string(app_config_path).expect("read current config"),
        "version 2\n"
    );
}

#[test]
fn migrate_write_failure_leaves_an_unchanged_file_out_of_the_already_migrated_list() {
    let config_directory = TempDir::new().unwrap();
    let keybinding_config_path = config_directory.path().join("keybinding.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&keybinding_config_path, "version 1\n").unwrap();
    fs::write(&app_config_path, "version 1\n").unwrap();

    let config_error = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_only_the_app_file,
        |_config_path, _serialized_config_bytes| {
            Err(StorageError::Io {
                detail: "injected failure".to_string(),
            })
        },
    )
    .config_report_text
    .unwrap_err();

    assert_eq!(
        config_error.to_string(),
        format!(
            "config failed: migration write failed for {}: storage io error: injected failure\n{} may already contain migrated data; check it before retrying",
            app_config_path.display(),
            app_config_path.display(),
        )
    );
    assert_eq!(
        fs::read_to_string(keybinding_config_path).unwrap(),
        "version 1\n"
    );
    assert_eq!(fs::read_to_string(app_config_path).unwrap(), "version 1\n");
}

/// `config_files`, sorted by path.
fn sort_config_files_by_path(mut config_files: Vec<ParsedConfigFile>) -> Vec<ParsedConfigFile> {
    config_files.sort_by(|left_config_file, right_config_file| {
        left_config_file
            .get_config_path()
            .cmp(right_config_file.get_config_path())
    });
    config_files
}

/// The command `koshi` parses from `cli_arguments`.
fn parse_cli_command(cli_arguments: &[&str]) -> CliCommand {
    <crate::cli::Cli as clap::Parser>::try_parse_from(cli_arguments)
        .expect("parse command")
        .command
        .expect("the arguments name a command")
}

/// A migration of the `koshi.kdl` at `app_config_path` read before the
/// migration lock: its text is `app_config_source_text`, and it migrates from
/// version 1 to version 2 as `migrated_app_config_text`.
fn build_earlier_app_config_migration(
    app_config_path: &Path,
    app_config_source_text: &str,
    migrated_app_config_text: &str,
) -> ConfigFileMigration {
    ConfigFileMigration {
        source_config_file: ParsedConfigFile::parse(
            app_config_path,
            app_config_source_text.to_string(),
        ),
        migration_result: Ok(MigratedConfig {
            source_schema_version: 1,
            target_schema_version: 2,
            migrated_config_file: Some(ParsedConfigFile::parse(
                app_config_path,
                migrated_app_config_text.to_string(),
            )),
        }),
    }
}

#[test]
fn migrate_takes_the_earlier_migration_of_a_file_whose_text_is_unchanged() {
    // The earlier migration of `version 1` gives `from-earlier-read #true`;
    // migrating the text again gives `version 2` alone.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        vec![build_earlier_app_config_migration(
            &app_config_path,
            "version 1\n",
            "version 2\nfrom-earlier-read #true\n",
        )],
        migrate_config,
        write_atomic,
    );

    assert_eq!(
        migration_report.config_report_text.unwrap(),
        format!("{}: migrated version 1 to 2\n", app_config_path.display())
    );
    assert_eq!(
        migration_report.config_files,
        vec![ParsedConfigFile::parse(
            &app_config_path,
            "version 2\nfrom-earlier-read #true\n".to_string()
        )]
    );
    assert_eq!(
        fs::read_to_string(&app_config_path).unwrap(),
        "version 2\nfrom-earlier-read #true\n"
    );
}

#[test]
fn migrate_parses_and_migrates_a_file_whose_text_changed_since_the_earlier_read() {
    // The earlier read found `edited-after-start #true`, which the text under the
    // lock no longer holds.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        vec![build_earlier_app_config_migration(
            &app_config_path,
            "version 1\nedited-after-start #true\n",
            "version 2\nfrom-earlier-read #true\n",
        )],
        migrate_config,
        write_atomic,
    );

    assert_eq!(
        migration_report.config_report_text.unwrap(),
        format!("{}: migrated version 1 to 2\n", app_config_path.display())
    );
    assert_eq!(
        migration_report.config_files,
        vec![ParsedConfigFile::parse(
            &app_config_path,
            "version 2\n".to_string()
        )]
    );
    assert_eq!(fs::read_to_string(&app_config_path).unwrap(), "version 2\n");
}

#[test]
fn migrate_stops_on_the_earlier_error_of_a_file_whose_text_is_unchanged() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();
    let earlier_config_migration = ConfigFileMigration {
        source_config_file: ParsedConfigFile::parse(&app_config_path, "version 1\n".to_string()),
        migration_result: Err(MigrationError::Invalid {
            config_path: app_config_path.display().to_string(),
            validation_error_detail: "a problem the earlier read found".to_string(),
        }),
    };

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        vec![earlier_config_migration],
        migrate_config,
        write_atomic,
    );

    assert_eq!(
        migration_report
            .config_report_text
            .map_err(|config_error| config_error.to_string()),
        Err(format!(
            "config failed: migration stopped before writing any file:\ninvalid config file {}: \
             a problem the earlier read found",
            app_config_path.display()
        ))
    );
    assert_eq!(
        migration_report.config_files,
        vec![ParsedConfigFile::parse(
            &app_config_path,
            "version 1\n".to_string()
        )]
    );
    assert_eq!(fs::read_to_string(&app_config_path).unwrap(), "version 1\n");
}

#[test]
fn migrate_stopped_before_writing_gives_every_file_read_as_read() {
    let config_directory = TempDir::new().unwrap();
    fs::create_dir(config_directory.path().join("themes")).unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let invalid_theme_path = config_directory.path().join("themes").join("bad.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();
    fs::write(&invalid_theme_path, "version 1\n").unwrap();

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_every_file_except_bad_kdl,
        write_atomic,
    );

    assert_eq!(
        sort_config_files_by_path(migration_report.config_files),
        vec![
            ParsedConfigFile::parse(&app_config_path, "version 1\n".to_string()),
            ParsedConfigFile::parse(&invalid_theme_path, "version 1\n".to_string()),
        ]
    );
}

#[test]
fn a_write_failure_gives_every_file_but_the_one_whose_write_failed() {
    let config_directory = TempDir::new().unwrap();
    fs::create_dir(config_directory.path().join("themes")).unwrap();
    let keybinding_config_path = config_directory.path().join("keybinding.kdl");
    let app_config_path = config_directory.path().join("koshi.kdl");
    let theme_config_path = config_directory.path().join("themes").join("plain.kdl");
    for config_path in [
        &keybinding_config_path,
        &app_config_path,
        &theme_config_path,
    ] {
        fs::write(config_path, "version 1\n").unwrap();
    }
    let mut write_attempt_count = 0;

    let migration_report = migrate_config_directory_with(
        config_directory.path(),
        Vec::new(),
        migrate_every_file_except_bad_kdl,
        |config_path, serialized_config_bytes| {
            write_attempt_count += 1;
            if write_attempt_count == 2 {
                return Err(StorageError::Io {
                    detail: "injected failure".to_string(),
                });
            }
            write_atomic(config_path, serialized_config_bytes)
        },
    );

    assert_eq!(write_attempt_count, 2);
    assert_eq!(
        sort_config_files_by_path(migration_report.config_files),
        vec![
            ParsedConfigFile::parse(
                &keybinding_config_path,
                "version 1\nmigrated #true\n".to_string()
            ),
            ParsedConfigFile::parse(&theme_config_path, "version 1\n".to_string()),
        ]
    );
}

#[test]
fn migrate_command_takes_the_koshi_kdl_read_at_start_when_its_text_is_unchanged() {
    // The read at start named the file `<directory>/./koshi.kdl`: the same
    // file under another spelling. The report names the path of the
    // migration it takes.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let start_read_app_config_path = config_directory.path().join(".").join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();

    let config_command_output = run_config_command_in_directory(
        &ConfigCommand::Migrate,
        config_directory.path(),
        &AppConfigOutcome::Absent,
        Some(ParsedConfigFile::parse(
            &start_read_app_config_path,
            "version 1\n".to_string(),
        )),
    )
    .unwrap();

    assert_eq!(
        config_command_output,
        format!(
            "{}: migrated version 1 to 2\n",
            start_read_app_config_path.display()
        )
    );
    assert_eq!(fs::read_to_string(&app_config_path).unwrap(), "version 2\n");
}

#[test]
fn migrate_command_parses_koshi_kdl_again_when_its_text_changed_since_start() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let start_read_app_config_path = config_directory.path().join(".").join("koshi.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();

    let config_command_output = run_config_command_in_directory(
        &ConfigCommand::Migrate,
        config_directory.path(),
        &AppConfigOutcome::Absent,
        Some(ParsedConfigFile::parse(
            &start_read_app_config_path,
            "version 1\nedited-after-start #true\n".to_string(),
        )),
    )
    .unwrap();

    assert_eq!(
        config_command_output,
        format!("{}: migrated version 1 to 2\n", app_config_path.display())
    );
    assert_eq!(fs::read_to_string(&app_config_path).unwrap(), "version 2\n");
}

#[test]
fn update_with_a_file_that_cannot_migrate_gives_every_file_as_read_and_takes_no_lock() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let keybinding_config_path = config_directory.path().join("keybinding.kdl");
    fs::write(&app_config_path, "version 1\n").unwrap();
    fs::write(&keybinding_config_path, "version 1\nnormal {").unwrap();
    fs::create_dir(config_directory.path().join(".migration.lock")).unwrap();

    let (config_files, config_migration_error) =
        migrate_config_directory_for_update(config_directory.path());

    assert_eq!(
        config_migration_error.map(|migration_error| migration_error.to_string()),
        Some(format!(
            "config failed: config parse error in {}: line 2, column 8: No closing '}}' for child \
             block",
            keybinding_config_path.display()
        ))
    );
    assert_eq!(
        config_files,
        vec![
            ParsedConfigFile::parse(&keybinding_config_path, "version 1\nnormal {".to_string()),
            ParsedConfigFile::parse(&app_config_path, "version 1\n".to_string()),
        ]
    );
    assert_eq!(fs::read_to_string(&app_config_path).unwrap(), "version 1\n");
}

#[test]
fn a_service_starts_on_the_koshi_kdl_its_migration_left_and_reads_no_file_again() {
    // `koshi.kdl` on disk is not KDL.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "pane {\n").unwrap();
    let app_config_text = "version 2\nallow-beta-features #true\n";

    let startup_config = build_service_startup_config(
        config_directory.path(),
        None,
        &[ParsedConfigFile::parse(
            &app_config_path,
            app_config_text.to_string(),
        )],
        None,
    );

    assert_eq!(
        startup_config.loaded_app_config,
        parse_app_config_text(&app_config_path, app_config_text.to_string())
    );
    assert_eq!(
        startup_config
            .config_migration_error
            .map(|migration_error| migration_error.to_string()),
        None
    );
    assert_eq!(
        startup_config
            .loaded_profile
            .map(|loaded_profile| loaded_profile.profile_name),
        None
    );
    assert_eq!(startup_config.app_config_file_for_migration, None);
}

#[test]
fn a_service_whose_migration_failed_starts_on_the_koshi_kdl_it_read_and_reads_no_file_again() {
    // `koshi.kdl` on disk is not KDL.
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(&app_config_path, "pane {\n").unwrap();
    let app_config_text = "version 2\nallow-beta-features #true\n";

    let startup_config = build_service_startup_config(
        config_directory.path(),
        None,
        &[ParsedConfigFile::parse(
            &app_config_path,
            app_config_text.to_string(),
        )],
        Some(CliError::Config {
            detail: "an injected migration failure".to_string(),
        }),
    );

    assert_eq!(
        startup_config.loaded_app_config,
        parse_app_config_text(&app_config_path, app_config_text.to_string())
    );
    assert_eq!(
        startup_config
            .config_migration_error
            .map(|migration_error| migration_error.to_string()),
        Some("config failed: an injected migration failure".to_string())
    );
}

#[test]
fn a_service_whose_failed_migration_left_no_koshi_kdl_reads_it_from_disk() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let app_config_text = "version 2\nallow-beta-features #true\n";
    fs::write(&app_config_path, app_config_text).unwrap();

    let startup_config = build_service_startup_config(
        config_directory.path(),
        None,
        &[],
        Some(CliError::Config {
            detail: "an injected migration failure".to_string(),
        }),
    );

    assert_eq!(
        startup_config.loaded_app_config,
        parse_app_config_text(&app_config_path, app_config_text.to_string())
    );
}

#[test]
fn a_service_whose_migration_found_no_koshi_kdl_starts_on_no_layer() {
    // A `koshi.kdl` written after the migration read the directory is not
    // read.
    let config_directory = TempDir::new().unwrap();
    fs::write(
        config_directory.path().join("koshi.kdl"),
        "version 2\nallow-beta-features #true\n",
    )
    .unwrap();

    let startup_config = build_service_startup_config(config_directory.path(), None, &[], None);

    assert_eq!(startup_config.loaded_app_config, LoadedAppConfig::default());
}

#[test]
fn a_seeded_session_opens_the_profile_its_migration_left_and_reads_no_file_again() {
    // `profile/work.kdl` on disk is not KDL.
    let config_directory = TempDir::new().unwrap();
    let profile_path = config_directory.path().join("profile").join("work.kdl");
    fs::create_dir(config_directory.path().join("profile")).unwrap();
    fs::write(&profile_path, "pane {").unwrap();
    let profile_text = "version 2\ntab { pane }\n";

    let startup_config = build_service_startup_config(
        config_directory.path(),
        Some("work"),
        &[ParsedConfigFile::parse(
            &profile_path,
            profile_text.to_string(),
        )],
        None,
    );

    let loaded_profile = startup_config
        .loaded_profile
        .expect("a seeded session with `--profile` reads its profile");
    assert_eq!(loaded_profile.profile_name, "work");
    let ProfileOutcome::Parsed { profile_template } = loaded_profile.profile_outcome else {
        panic!(
            "the profile the migration left parses: {:?}",
            loaded_profile.profile_outcome
        );
    };
    assert_eq!(
        profile_template,
        parse_profile(&profile_path, profile_text).unwrap()
    );
}

#[test]
fn a_seeded_session_start_that_migrates_opens_the_migrated_koshi_kdl_and_profile() {
    let config_directory = TempDir::new().unwrap();
    let app_config_path = config_directory.path().join("koshi.kdl");
    let profile_path = config_directory.path().join("profile").join("work.kdl");
    fs::write(&app_config_path, "version 1\nallow-beta-features #true\n").unwrap();
    fs::create_dir(config_directory.path().join("profile")).unwrap();
    fs::write(&profile_path, "version 1\ntab { pane }\n").unwrap();
    let session_id_text = koshi_core::ids::SessionId::new().to_string();
    let serve_session_command = parse_cli_command(&[
        "koshi",
        "serve-session",
        &session_id_text,
        "workspace",
        "--profile",
        "work",
    ]);

    let startup_config =
        load_config_for_command(Some(&serve_session_command), Some(config_directory.path()));

    assert_eq!(
        startup_config
            .config_migration_error
            .map(|migration_error| migration_error.to_string()),
        None
    );
    assert_eq!(
        startup_config.loaded_app_config,
        parse_app_config_text(
            &app_config_path,
            "version 2\nallow-beta-features #true\n".to_string()
        )
    );
    let loaded_profile = startup_config
        .loaded_profile
        .expect("a seeded session with `--profile` reads its profile");
    let ProfileOutcome::Parsed { profile_template } = loaded_profile.profile_outcome else {
        panic!(
            "the migrated profile parses: {:?}",
            loaded_profile.profile_outcome
        );
    };
    assert_eq!(
        profile_template,
        parse_profile(&profile_path, "version 2\ntab { pane }\n").unwrap()
    );
    assert_eq!(
        fs::read_to_string(&profile_path).unwrap(),
        "version 2\ntab { pane }\n"
    );
}

#[test]
fn only_a_session_that_seeds_itself_reads_a_profile() {
    let config_directory = TempDir::new().unwrap();
    fs::create_dir(config_directory.path().join("profile")).unwrap();
    fs::write(
        config_directory.path().join("profile").join("work.kdl"),
        "version 2\ntab { pane }\n",
    )
    .unwrap();
    let session_id_text = koshi_core::ids::SessionId::new().to_string();
    let resumed_session_command = parse_cli_command(&[
        "koshi",
        "serve-session",
        &session_id_text,
        "workspace",
        "--profile",
        "work",
        "--resume",
        "/home/user/resume-state",
    ]);
    let session_command_without_profile =
        parse_cli_command(&["koshi", "serve-session", &session_id_text, "workspace"]);
    let seeded_session_command = parse_cli_command(&[
        "koshi",
        "serve-session",
        &session_id_text,
        "workspace",
        "--profile",
        "work",
    ]);
    let router_command = parse_cli_command(&["koshi", "serve-router"]);
    let check_command = parse_cli_command(&["koshi", "config", "check"]);

    for (cli_command, config_directory) in [
        (&resumed_session_command, Some(config_directory.path())),
        (
            &session_command_without_profile,
            Some(config_directory.path()),
        ),
        (&router_command, Some(config_directory.path())),
        (&check_command, Some(config_directory.path())),
        (&seeded_session_command, None),
    ] {
        let startup_config = load_config_for_command(Some(cli_command), config_directory);

        assert_eq!(
            startup_config
                .loaded_profile
                .map(|loaded_profile| loaded_profile.profile_name),
            None,
            "{cli_command:?}"
        );
    }
}

#[test]
fn config_migrate_starts_on_the_koshi_kdl_every_other_command_starts_on() {
    let config_migrate_command = parse_cli_command(&["koshi", "config", "migrate"]);
    let config_check_command = parse_cli_command(&["koshi", "config", "check"]);
    for app_config_text in [
        None,
        Some("version 2\nallow-beta-features #true\n"),
        Some("version 1\nallow-beta-features #true\n"),
        Some("pane {\n"),
    ] {
        let config_directory = TempDir::new().unwrap();
        let app_config_path = config_directory.path().join("koshi.kdl");
        if let Some(app_config_text) = app_config_text {
            fs::write(&app_config_path, app_config_text).unwrap();
        }

        let startup_config =
            load_config_for_command(Some(&config_migrate_command), Some(config_directory.path()));

        assert_eq!(
            startup_config.loaded_app_config,
            load_config_for_command(Some(&config_check_command), Some(config_directory.path()))
                .loaded_app_config,
            "{app_config_text:?}"
        );
        assert_eq!(
            startup_config.app_config_file_for_migration,
            app_config_text.map(|app_config_text| ParsedConfigFile::parse(
                &app_config_path,
                app_config_text.to_string()
            )),
            "{app_config_text:?}"
        );
        assert_eq!(
            startup_config
                .config_migration_error
                .map(|migration_error| migration_error.to_string()),
            None
        );
    }

    let unreadable_config_directory = TempDir::new().unwrap();
    fs::create_dir(unreadable_config_directory.path().join("koshi.kdl")).unwrap();
    let startup_config = load_config_for_command(
        Some(&config_migrate_command),
        Some(unreadable_config_directory.path()),
    );
    assert_eq!(
        startup_config.loaded_app_config,
        load_config_for_command(
            Some(&config_check_command),
            Some(unreadable_config_directory.path())
        )
        .loaded_app_config
    );
    assert_eq!(startup_config.app_config_file_for_migration, None);

    let startup_config = load_config_for_command(Some(&config_migrate_command), None);
    assert_eq!(
        startup_config.loaded_app_config,
        load_config_for_command(Some(&config_check_command), None).loaded_app_config
    );
    assert_eq!(startup_config.app_config_file_for_migration, None);
}
