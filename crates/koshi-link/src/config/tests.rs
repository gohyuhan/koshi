//! Tests for `config`: file-name validation, the per-file readers, the beta
//! gate, the logging parameters, the new-pane direction, the other-users
//! policy, and native image output.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use koshi_beta::beta_feature;
use koshi_config::layer::PartialLoggingConfig;
use koshi_config::types::{BoundAction, ModeBindings, ModeName, RgbColor};
use koshi_core::action::ActionReference;
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, KeySequence};
use koshi_core::log::{LogFormat, LogLevel};

use super::*;

/// A beta-gated entry point: returns `1` when the gate is open and `0` when it
/// is closed.
#[beta_feature(otherwise = 0)]
fn mock_beta_entry_point() -> u32 {
    1
}

#[test]
fn a_plain_name_is_accepted() {
    assert!(is_plain_file_name("dev"));
    assert!(is_plain_file_name("work.2"));
    assert!(is_plain_file_name("my-profile"));
    assert!(is_plain_file_name("midnight"));
}

#[test]
fn a_path_traversing_or_absolute_name_is_rejected() {
    // Each of these joins to a `.kdl` outside `profile/` or `themes/`.
    assert!(!is_plain_file_name("../secret"));
    assert!(!is_plain_file_name("a/b"));
    assert!(!is_plain_file_name("/etc/passwd"));
    assert!(!is_plain_file_name(".."));
    assert!(!is_plain_file_name("."));
    assert!(!is_plain_file_name(""));
}

#[test]
fn a_nested_or_trailing_separator_name_is_rejected() {
    // `foo/` joins to the nested file `profile/foo/.kdl`, and `foo/..` ends in
    // a `..` component.
    assert!(!is_plain_file_name("foo/"));
    assert!(!is_plain_file_name("foo/.."));
}

#[test]
fn a_leading_or_embedded_dot_name_stays_plain() {
    // Only the exact `.` and `..` components are rejected. A leading dot or a
    // double dot inside a longer name is an ordinary flat file name.
    assert!(is_plain_file_name(".hidden"));
    assert!(is_plain_file_name("a..b"));
    assert!(is_plain_file_name("..config"));
    assert!(is_plain_file_name("config.."));
}

#[test]
fn a_space_or_non_ascii_name_stays_plain() {
    // A space and a non-ASCII character are path separators on no platform:
    // each name joins to a `.kdl` directly under `profile/` or `themes/`.
    assert!(is_plain_file_name(" "));
    assert!(is_plain_file_name("my profile"));
    assert!(is_plain_file_name("日本語"));
    assert!(is_plain_file_name("thème"));
}

#[test]
fn a_backslash_in_a_name_follows_the_platform_separator() {
    // A backslash is a path separator on Windows, where `a\b` is rejected. On
    // Unix it is an ordinary character, and `a\b` stays plain.
    #[cfg(windows)]
    assert!(!is_plain_file_name("a\\b"));
    #[cfg(not(windows))]
    assert!(is_plain_file_name("a\\b"));
}

// --- load_config_file: absent, present, and unreadable files ---

#[test]
fn reading_an_absent_file_is_none_without_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_config_file(
            &test_directory.path().join("missing.kdl"),
            &mut config_warnings
        ),
        None
    );
    assert_eq!(config_warnings, Vec::<String>::new());
}

#[test]
fn reading_a_present_file_returns_its_exact_text() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("present.kdl");
    fs::write(&config_file_path, "version 1\n").expect("write");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_config_file(&config_file_path, &mut config_warnings),
        Some("version 1\n".to_string())
    );
    assert_eq!(config_warnings, Vec::<String>::new());
}

#[test]
fn reading_a_directory_as_a_file_warns_and_is_none() {
    // Reading a directory as a string fails on every platform.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let read_error =
        fs::read_to_string(test_directory.path()).expect_err("a directory reads as no string");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_config_file(test_directory.path(), &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "could not read config file {}: {read_error}",
            test_directory.path().display()
        )]
    );
}

/// `present.kdl/keybinding.kdl` has a regular file as its parent. On Unix the
/// read fails with "not a directory", which is not an absent file.
#[cfg(unix)]
#[test]
fn reading_a_path_below_a_regular_file_warns_and_is_none() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let parent_file_path = test_directory.path().join("present.kdl");
    fs::write(&parent_file_path, "version 1\n").expect("write");
    let config_file_path = parent_file_path.join("keybinding.kdl");
    let read_error =
        fs::read_to_string(&config_file_path).expect_err("a regular file holds no entries");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_config_file(&config_file_path, &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "could not read config file {}: {read_error}",
            config_file_path.display()
        )]
    );
}

#[test]
fn reading_an_empty_file_returns_an_empty_string_without_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("empty.kdl");
    fs::write(&config_file_path, "").expect("write");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_config_file(&config_file_path, &mut config_warnings),
        Some(String::new())
    );
    assert_eq!(config_warnings, Vec::<String>::new());
}

#[test]
fn reading_a_file_that_is_not_utf8_warns_and_is_none() {
    // `0x80` starts no UTF-8 sequence: the read fails on every platform.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("invalid.kdl");
    fs::write(&config_file_path, [0x76, 0x65, 0x80, 0x72]).expect("write");
    let read_error = fs::read_to_string(&config_file_path).expect_err("the bytes are not UTF-8");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_config_file(&config_file_path, &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "could not read config file {}: {read_error}",
            config_file_path.display()
        )]
    );
}

// --- load_app_config: clean, absent, hard-error, and unknown-field files ---

#[test]
fn loading_a_clean_app_file_returns_a_layer_without_warnings() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    fs::write(&config_file_path, "version 1\n").expect("write");
    let mut config_warnings = Vec::new();
    let app_config_file =
        load_app_config(&config_file_path, &mut config_warnings).expect("the file loads");
    assert_eq!(app_config_file.layer, PartialKoshiConfig::default());
    assert_eq!(app_config_file.theme_name, None);
    assert_eq!(app_config_file.parse_warnings, Vec::<String>::new());
    assert_eq!(config_warnings, Vec::<String>::new());
}

#[test]
fn loading_an_absent_app_file_is_none_without_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_app_config(
            &test_directory.path().join("koshi.kdl"),
            &mut config_warnings
        ),
        None
    );
    assert_eq!(config_warnings, Vec::<String>::new());
}

#[test]
fn an_app_file_with_an_unsupported_version_drops_to_defaults_with_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    fs::write(&config_file_path, "version 999\n").expect("write");
    let parse_error =
        parse_app_config(&config_file_path, "version 999\n").expect_err("version 999 is refused");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_app_config(&config_file_path, &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "koshi.kdl not applied ({}): {parse_error}; using defaults",
            config_file_path.display()
        )]
    );
}

#[test]
fn an_empty_app_file_drops_to_defaults_with_a_warning() {
    // An empty file names no `version`, which `parse_app_config` refuses.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    fs::write(&config_file_path, "").expect("write");
    let parse_error =
        parse_app_config(&config_file_path, "").expect_err("a missing version is refused");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_app_config(&config_file_path, &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "koshi.kdl not applied ({}): {parse_error}; using defaults",
            config_file_path.display()
        )]
    );
}

#[test]
fn an_unknown_app_field_is_kept_as_a_path_prefixed_skip_warning() {
    // A `koshi.kdl` that parses but names an unknown key applies its other
    // fields and records the skip, prefixed with the file it came from.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    fs::write(
        &config_file_path,
        "version 1\nfrobnicate 1\ntheme \"midnight\"\n",
    )
    .expect("write");
    let mut config_warnings = Vec::new();
    let app_config_file =
        load_app_config(&config_file_path, &mut config_warnings).expect("the file loads");
    assert_eq!(app_config_file.theme_name, Some("midnight".to_string()));
    assert_eq!(
        config_warnings,
        vec![format!(
            "{}: ignored unknown key `frobnicate`; did you mean `update`?",
            config_file_path.display()
        )]
    );
}

// --- load_theme_config: selecting a `themes/<name>.kdl` by name ---

/// Writes `theme_source` to `themes/<theme_name>.kdl` under `config_directory`,
/// creates the theme directory, and returns the theme file path.
fn write_theme_file(config_directory: &Path, theme_name: &str, theme_source: &str) -> PathBuf {
    let themes_directory = config_directory.join("themes");
    fs::create_dir_all(&themes_directory).expect("create themes dir");
    let theme_file_path = themes_directory.join(format!("{theme_name}.kdl"));
    fs::write(&theme_file_path, theme_source).expect("write");
    theme_file_path
}

#[test]
fn a_selected_theme_is_read_from_the_themes_directory_and_named_after_its_file() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    write_theme_file(
        test_directory.path(),
        "midnight",
        "version 1\ncolors {\n    accent \"#f5c2ff\"\n}\n",
    );
    let mut config_warnings = Vec::new();
    let theme_config_layer =
        load_theme_config(test_directory.path(), "midnight", &mut config_warnings)
            .expect("theme loads");
    assert_eq!(theme_config_layer.theme_name, Some("midnight".to_string()));
    assert_eq!(
        theme_config_layer.colors.expect("colors set").accent,
        Some(RgbColor {
            red: 0xf5,
            green: 0xc2,
            blue: 0xff
        })
    );
    assert_eq!(config_warnings, Vec::<String>::new());
}

#[test]
fn selecting_the_default_theme_by_name_keeps_the_built_in_colors_silently() {
    // `default` names the built-in theme: no file is read and no warning is
    // recorded.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(
            test_directory.path(),
            DEFAULT_THEME_NAME,
            &mut config_warnings
        ),
        None
    );
    assert_eq!(config_warnings, Vec::<String>::new());
}

#[test]
fn a_default_named_theme_file_is_ignored_in_favor_of_the_built_in_theme() {
    // Even with a `themes/default.kdl` on disk, the reserved name means the
    // built-in colors: the file is never read.
    let test_directory = tempfile::tempdir().expect("temp dir");
    write_theme_file(
        test_directory.path(),
        DEFAULT_THEME_NAME,
        "colors {\n    accent \"#ff0000\"\n}\n",
    );
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(
            test_directory.path(),
            DEFAULT_THEME_NAME,
            &mut config_warnings
        ),
        None
    );
    assert_eq!(config_warnings, Vec::<String>::new());
}

#[test]
fn a_theme_with_no_file_falls_back_to_the_default_with_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "missing", &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "theme `missing` not found at {}; using the default theme",
            test_directory
                .path()
                .join("themes")
                .join("missing.kdl")
                .display()
        )]
    );
}

#[test]
fn a_path_traversing_theme_name_is_rejected_before_any_file_is_read() {
    // `theme "../../secret"` reads no file.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "../../secret", &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec!["theme name `../../secret` must be a plain name; using the default theme".to_string()]
    );
}

#[test]
fn an_empty_theme_name_is_rejected_before_any_file_is_read() {
    // `""` is not a plain name: no file under `themes/` is read.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "", &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec!["theme name `` must be a plain name; using the default theme".to_string()]
    );
}

#[test]
fn an_unknown_theme_field_is_kept_as_a_path_prefixed_skip_warning() {
    // A theme file that parses but names an unknown color role applies its other
    // fields and records the skip, prefixed with the file it came from.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let theme_file_path = write_theme_file(
        test_directory.path(),
        "midnight",
        "version 1\ncolors {\n    foreground \"#ffffff\"\n}\n",
    );
    let mut config_warnings = Vec::new();
    let theme_config_layer =
        load_theme_config(test_directory.path(), "midnight", &mut config_warnings)
            .expect("the theme loads");
    assert_eq!(theme_config_layer.theme_name, Some("midnight".to_string()));
    assert_eq!(
        config_warnings,
        vec![format!(
            "{}: ignored unknown key `colors.foreground`; did you mean `colors.ramp-end`?",
            theme_file_path.display()
        )]
    );
}

#[test]
fn a_theme_file_with_an_unsupported_version_falls_back_to_the_default_with_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let theme_file_path = write_theme_file(test_directory.path(), "midnight", "version 999\n");
    let parse_error =
        parse_theme(&theme_file_path, "version 999\n").expect_err("version 999 is refused");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "midnight", &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "theme `midnight` not applied ({}): {parse_error}; using the default theme",
            theme_file_path.display()
        )]
    );
}

#[test]
fn an_unreadable_theme_file_reports_the_cause_and_the_fallback_in_one_line() {
    // A directory named `midnight.kdl` fails to read with an error other than
    // `NotFound` on every platform. One warning carries the path, the OS
    // reason, and the fallback theme.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let theme_file_path = test_directory.path().join("themes").join("midnight.kdl");
    fs::create_dir_all(&theme_file_path).expect("create dir in place of the file");
    let read_error =
        fs::read_to_string(&theme_file_path).expect_err("a directory reads as no string");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "midnight", &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "theme `midnight` could not be read ({}): {read_error}; using the default theme",
            theme_file_path.display()
        )]
    );
}

#[test]
fn a_theme_missing_from_an_existing_themes_directory_is_reported_as_not_found() {
    // An absent theme file gives the "not found" warning, never the "could not
    // be read" one.
    let test_directory = tempfile::tempdir().expect("temp dir");
    fs::create_dir_all(test_directory.path().join("themes")).expect("create themes dir");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "midnight", &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "theme `midnight` not found at {}; using the default theme",
            test_directory
                .path()
                .join("themes")
                .join("midnight.kdl")
                .display()
        )]
    );
}

// --- load_keybindings_config: valid and unparseable files ---

#[test]
fn loading_a_valid_keybinding_file_returns_a_layer_without_warnings() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("keybinding.kdl");
    fs::write(
        &config_file_path,
        "version 1\nmode \"normal\" {\n    bind \"<C-y>\" \"core:new-tab\"\n}\n",
    )
    .expect("write");
    let mut config_warnings = Vec::new();
    let keybindings_config_layer =
        load_keybindings_config(&config_file_path, &mut config_warnings).expect("the file loads");
    assert_eq!(keybindings_config_layer.chord_timeout_ms, None);
    assert_eq!(keybindings_config_layer.which_key_delay_ms, None);
    assert_eq!(keybindings_config_layer.maximum_chord_depth, None);
    assert_eq!(keybindings_config_layer.leader, None);
    assert_eq!(keybindings_config_layer.unlock_alternative, None);
    assert_eq!(
        keybindings_config_layer.mode_bindings_by_name,
        Some(BTreeMap::from([(
            ModeName::from_text("normal"),
            ModeBindings {
                bound_action_by_key_sequence: BTreeMap::from([(
                    KeySequence::from(KeyChord::from_parts(
                        BindingModifierFlags::CTRL,
                        Key::Char('y')
                    )),
                    BoundAction {
                        action_reference: ActionReference::from_core_action_name("new-tab")
                            .expect("a core action name"),
                    },
                )]),
                removed_key_sequences: BTreeSet::new(),
            },
        )]))
    );
    assert_eq!(config_warnings, Vec::<String>::new());
}

#[test]
fn loading_an_absent_keybinding_file_is_none_without_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_keybindings_config(
            &test_directory.path().join("keybinding.kdl"),
            &mut config_warnings
        ),
        None
    );
    assert_eq!(config_warnings, Vec::<String>::new());
}

#[test]
fn an_unparseable_keybinding_file_drops_the_whole_file_with_a_warning() {
    // `keybinding.kdl` is all-or-nothing: any parse error drops the file.
    let keybinding_source = "mode \"normal\" {\n    bind \"<C-\" \"core:new-tab\"\n}\n";
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("keybinding.kdl");
    fs::write(&config_file_path, keybinding_source).expect("write");
    let parse_error = parse_keybindings(&config_file_path, keybinding_source)
        .expect_err("the file does not parse");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_keybindings_config(&config_file_path, &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "keybinding.kdl not applied ({}): {parse_error}; using defaults",
            config_file_path.display()
        )]
    );
}

// --- append_config_field_warnings: file-prefixed skip lines ---

#[test]
fn append_config_field_warnings_prefixes_each_skip_with_the_file_path() {
    let config_file_path = Path::new("some/koshi.kdl");
    let mut config_warnings = vec!["earlier".to_string()];
    append_config_field_warnings(
        config_file_path,
        &["first skip".to_string(), "second skip".to_string()],
        &mut config_warnings,
    );
    assert_eq!(
        config_warnings,
        vec![
            "earlier".to_string(),
            format!("{}: first skip", config_file_path.display()),
            format!("{}: second skip", config_file_path.display()),
        ]
    );
}

#[test]
fn append_config_field_warnings_adds_nothing_for_an_empty_skip_list() {
    let config_file_path = Path::new("themes/midnight.kdl");
    let mut config_warnings = Vec::new();
    append_config_field_warnings(config_file_path, &[], &mut config_warnings);
    assert_eq!(config_warnings, Vec::<String>::new());
}

/// `apply_beta_gate` opens the process-wide gate for `allow-beta-features
/// #true` and closes it for `#false` and for no `koshi.kdl` at all.
#[test]
fn apply_beta_gate_opens_the_gate_only_when_the_file_asks_for_it() {
    let enabled_config = PartialKoshiConfig {
        should_allow_beta_features: Some(true),
        ..Default::default()
    };
    let disabled_config = PartialKoshiConfig {
        should_allow_beta_features: Some(false),
        ..Default::default()
    };

    apply_beta_gate(Some(enabled_config.clone()));
    assert!(koshi_beta::should_allow_beta_features());

    apply_beta_gate(Some(disabled_config));
    assert!(!koshi_beta::should_allow_beta_features());

    // No `koshi.kdl` at all closes an open gate.
    apply_beta_gate(Some(enabled_config));
    assert!(koshi_beta::should_allow_beta_features());
    apply_beta_gate(None);
    assert!(!koshi_beta::should_allow_beta_features());

    // Text on disk → `load_app_config` → `apply_beta_gate` → a
    // `#[beta_feature]` function.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");

    fs::write(&config_file_path, "version 2\nallow-beta-features #true\n").expect("write");
    let mut config_warnings = Vec::new();
    apply_beta_gate(
        load_app_config(&config_file_path, &mut config_warnings)
            .map(|app_config_file| app_config_file.layer),
    );
    assert_eq!(config_warnings, Vec::<String>::new());
    assert_eq!(mock_beta_entry_point(), 1);

    fs::write(&config_file_path, "version 2\nallow-beta-features #false\n").expect("write");
    let mut config_warnings = Vec::new();
    apply_beta_gate(
        load_app_config(&config_file_path, &mut config_warnings)
            .map(|app_config_file| app_config_file.layer),
    );
    assert_eq!(config_warnings, Vec::<String>::new());
    assert_eq!(mock_beta_entry_point(), 0);
}

#[test]
fn build_logging_parameters_with_no_config_file_are_the_defaults() {
    let session_id = SessionId::new();
    let logging_parameters = build_logging_parameters(None, session_id);

    assert!(!logging_parameters.is_enabled);
    assert_eq!(logging_parameters.log_level, LogLevel::Warning);
    assert_eq!(logging_parameters.log_format, LogFormat::Pretty);
    assert_eq!(logging_parameters.session_id, session_id);
}

#[test]
fn build_logging_parameters_take_the_level_and_format_the_config_names() {
    let session_id = SessionId::new();
    let app_config = PartialKoshiConfig {
        logging: Some(PartialLoggingConfig {
            is_enabled: Some(true),
            log_level: Some(LogLevel::Info),
            log_format: Some(LogFormat::Json),
        }),
        ..Default::default()
    };

    let logging_parameters = build_logging_parameters(Some(&app_config), session_id);

    assert!(logging_parameters.is_enabled);
    assert_eq!(logging_parameters.log_level, LogLevel::Info);
    assert_eq!(logging_parameters.log_format, LogFormat::Json);
    assert_eq!(logging_parameters.session_id, session_id);
}

#[test]
fn build_logging_parameters_keep_the_defaults_for_every_field_the_config_leaves_out() {
    let session_id = SessionId::new();
    let app_config = PartialKoshiConfig {
        logging: Some(PartialLoggingConfig {
            is_enabled: Some(true),
            log_level: None,
            log_format: None,
        }),
        ..Default::default()
    };

    let logging_parameters = build_logging_parameters(Some(&app_config), session_id);

    assert!(logging_parameters.is_enabled);
    assert_eq!(logging_parameters.log_level, LogLevel::Warning);
    assert_eq!(logging_parameters.log_format, LogFormat::Pretty);
    assert_eq!(logging_parameters.session_id, session_id);
}

// --- The direction a pane-opening verb uses with no `--direction` ---

#[test]
fn a_pane_with_no_direction_named_anywhere_opens_rightward() {
    assert_eq!(resolve_new_pane_direction(None), Direction::Right);
}

#[test]
fn a_pane_with_no_direction_named_takes_the_one_the_config_names() {
    let app_config = PartialKoshiConfig {
        layout: Some(koshi_config::layer::PartialLayoutDefaults {
            new_pane_direction: Some(Direction::Left),
        }),
        ..Default::default()
    };

    assert_eq!(
        resolve_new_pane_direction(Some(app_config)),
        Direction::Left
    );
}

#[test]
fn a_layout_section_naming_no_direction_still_opens_rightward() {
    let app_config = PartialKoshiConfig {
        layout: Some(koshi_config::layer::PartialLayoutDefaults {
            new_pane_direction: None,
        }),
        ..Default::default()
    };

    assert_eq!(
        resolve_new_pane_direction(Some(app_config)),
        Direction::Right
    );
}

// --- Who may reach a session's control socket ---

/// A `koshi.kdl` layer with `should_allow_other_users` and no shared directory.
fn build_other_user_access_config_layer(should_allow_other_users: bool) -> PartialKoshiConfig {
    PartialKoshiConfig {
        should_allow_other_users: Some(should_allow_other_users),
        ..Default::default()
    }
}

/// A `koshi.kdl` layer with `should_allow_other_users` and
/// `shared_sessions_directory`.
fn build_other_user_access_config_layer_with_shared_sessions_directory(
    should_allow_other_users: bool,
    shared_sessions_directory: &str,
) -> PartialKoshiConfig {
    PartialKoshiConfig {
        should_allow_other_users: Some(should_allow_other_users),
        shared_sessions_directory: Some(Some(PathBuf::from(shared_sessions_directory))),
        ..Default::default()
    }
}

/// The directory `other_users_policy` shares through, or `None` when the
/// session serves only the user who started it.
fn get_shared_sessions_directory(other_users_policy: Option<OtherUsers>) -> Option<PathBuf> {
    other_users_policy.map(|other_users_access_policy| other_users_access_policy.shared_directory)
}

#[test]
fn a_fresh_install_serves_only_the_user_who_started_the_session() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(None, false, None)),
        None
    );
}

#[test]
fn a_config_turning_the_switch_off_serves_only_that_user() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(
            Some(&build_other_user_access_config_layer(false)),
            false,
            None,
        )),
        None
    );
}

#[test]
fn a_config_turning_the_switch_on_shares_through_the_machine_wide_directory() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(
            Some(&build_other_user_access_config_layer(true)),
            false,
            None,
        )),
        koshi_paths::resolve_shared_sessions_directory()
    );
}

#[test]
fn a_config_naming_a_shared_directory_shares_through_that_one() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(
            Some(
                &build_other_user_access_config_layer_with_shared_sessions_directory(
                    true,
                    "/var/run/koshi"
                )
            ),
            false,
            None
        )),
        Some(PathBuf::from("/var/run/koshi"))
    );
}

#[test]
fn naming_a_shared_directory_alone_serves_only_this_user() {
    // `shared-sessions-dir` without `allow-other-users #true` shares nothing.
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(
            Some(
                &build_other_user_access_config_layer_with_shared_sessions_directory(
                    false,
                    "/var/run/koshi"
                )
            ),
            false,
            None
        )),
        None
    );
}

#[test]
fn the_flag_shares_a_session_whose_config_says_no() {
    let other_users_policy = resolve_other_users_policy(
        Some(
            &build_other_user_access_config_layer_with_shared_sessions_directory(
                false,
                "/var/run/koshi",
            ),
        ),
        true,
        None,
    )
    .expect("the flag turns the switch on");

    assert_eq!(
        other_users_policy.shared_directory,
        PathBuf::from("/var/run/koshi")
    );
    // Under the flag, `is_enabled` returns `true` on every call, whatever the
    // app file says.
    assert!((other_users_policy.is_enabled)());
    assert!((other_users_policy.is_enabled)());
}

#[test]
fn the_flag_shares_a_session_that_has_no_config_file_at_all() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(None, true, None)),
        koshi_paths::resolve_shared_sessions_directory()
    );
}

// --- Whether a viewer sends native image output ---

#[test]
fn supports_image_output_with_no_app_file_is_on() {
    assert!(supports_image_output(None));
}

#[test]
fn supports_image_output_takes_the_value_the_app_file_names() {
    let disabled_config = PartialKoshiConfig {
        supports_image_protocols: Some(false),
        ..Default::default()
    };
    assert!(!supports_image_output(Some(disabled_config)));

    let unset_config = PartialKoshiConfig::default();
    assert!(supports_image_output(Some(unset_config)));
}

// --- Reading the config directory a caller names ---

/// A fresh config directory holding a `koshi.kdl` with `app_config_text`.
fn build_config_directory_with_app_config(app_config_text: &str) -> tempfile::TempDir {
    let config_directory = tempfile::tempdir().expect("temp dir");
    fs::write(config_directory.path().join("koshi.kdl"), app_config_text).expect("write koshi.kdl");
    config_directory
}

#[test]
fn no_config_directory_reads_no_file_and_keeps_every_default() {
    let (loaded_config, config_warnings) = load_config_files(None);

    assert_eq!(loaded_config.app_config_layer, None);
    assert_eq!(loaded_config.theme_config_layer, None);
    assert_eq!(loaded_config.keybindings_config_layer, None);
    assert_eq!(
        config_warnings,
        vec!["no config directory found; using built-in defaults".to_string()]
    );
    assert_eq!(load_app_layer(None), None);
    assert_eq!(load_profile_template(None, "work"), None);
    assert_eq!(find_shared_sessions_base_directory(None), None);
}

#[test]
fn a_config_directory_turning_the_switch_on_names_its_shared_directory() {
    let config_directory = build_config_directory_with_app_config(
        "version 2\nallow-other-users #true\nshared-sessions-dir \"/var/run/koshi\"\n",
    );

    assert_eq!(
        find_shared_sessions_base_directory(Some(config_directory.path())),
        Some(PathBuf::from("/var/run/koshi"))
    );
}

#[test]
fn a_config_directory_leaving_the_switch_off_names_no_shared_directory() {
    let config_directory = build_config_directory_with_app_config(
        "version 2\nshared-sessions-dir \"/var/run/koshi\"\n",
    );

    assert_eq!(
        find_shared_sessions_base_directory(Some(config_directory.path())),
        None
    );
}

#[test]
fn a_switch_left_to_the_file_is_read_from_the_config_directory_on_every_call() {
    let config_directory = build_config_directory_with_app_config(
        "version 2\nallow-other-users #true\nshared-sessions-dir \"/var/run/koshi\"\n",
    );
    let other_users_policy = resolve_other_users_policy(
        load_app_layer(Some(config_directory.path())).as_ref(),
        false,
        Some(config_directory.path()),
    )
    .expect("the file turns the switch on");
    assert!((other_users_policy.is_enabled)());

    fs::write(
        config_directory.path().join("koshi.kdl"),
        "version 2\nallow-other-users #false\nshared-sessions-dir \"/var/run/koshi\"\n",
    )
    .expect("rewrite koshi.kdl");

    assert!(!(other_users_policy.is_enabled)());
}
