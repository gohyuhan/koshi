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
use koshi_core::ids::SessionId;
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

// --- read_config_file: absent, present, and unreadable files ---

/// `read_config_file`'s answer with the read error as its text.
fn read_config_file_as_text(config_path: &Path) -> Result<Option<String>, String> {
    read_config_file(config_path).map_err(|read_error| read_error.to_string())
}

#[test]
fn reading_an_absent_file_is_none() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    assert_eq!(
        read_config_file_as_text(&test_directory.path().join("missing.kdl")),
        Ok(None)
    );
}

#[test]
fn reading_a_present_file_returns_its_exact_text() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("present.kdl");
    fs::write(&config_file_path, "version 1\n").expect("write");
    assert_eq!(
        read_config_file_as_text(&config_file_path),
        Ok(Some("version 1\n".to_string()))
    );
}

#[test]
fn reading_a_directory_as_a_file_is_its_read_error() {
    // Reading a directory as a string fails on every platform.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let read_error =
        fs::read_to_string(test_directory.path()).expect_err("a directory reads as no string");
    assert_eq!(
        read_config_file_as_text(test_directory.path()),
        Err(read_error.to_string())
    );
}

/// `present.kdl/keybinding.kdl` has a regular file as its parent. On Unix the
/// read fails with "not a directory", which is not an absent file.
#[cfg(unix)]
#[test]
fn reading_a_path_below_a_regular_file_is_its_read_error() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let parent_file_path = test_directory.path().join("present.kdl");
    fs::write(&parent_file_path, "version 1\n").expect("write");
    let config_file_path = parent_file_path.join("keybinding.kdl");
    let read_error =
        fs::read_to_string(&config_file_path).expect_err("a regular file holds no entries");
    assert_eq!(
        read_config_file_as_text(&config_file_path),
        Err(read_error.to_string())
    );
}

#[test]
fn reading_an_empty_file_returns_an_empty_string() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("empty.kdl");
    fs::write(&config_file_path, "").expect("write");
    assert_eq!(
        read_config_file_as_text(&config_file_path),
        Ok(Some(String::new()))
    );
}

#[test]
fn reading_a_file_that_is_not_utf8_is_its_read_error() {
    // `0x80` starts no UTF-8 sequence: the read fails on every platform.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("invalid.kdl");
    fs::write(&config_file_path, [0x76, 0x65, 0x80, 0x72]).expect("write");
    let read_error = fs::read_to_string(&config_file_path).expect_err("the bytes are not UTF-8");
    assert_eq!(
        read_config_file_as_text(&config_file_path),
        Err(read_error.to_string())
    );
}

// --- load_app_config_at: clean, absent, unreadable, hard-error, and unknown-field files ---

#[test]
fn loading_a_clean_app_file_returns_a_layer_without_warnings() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    fs::write(&config_file_path, "version 1\n").expect("write");
    assert_eq!(
        load_app_config_at(&config_file_path),
        LoadedAppConfig {
            app_config_outcome: AppConfigOutcome::Parsed {
                app_config_layer: Box::new(PartialKoshiConfig::default()),
                theme_name: None,
                app_config_source_text: "version 1\n".to_string(),
            },
            config_warnings: Vec::new(),
        }
    );
}

#[test]
fn loading_an_absent_app_file_is_absent_without_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    assert_eq!(
        load_app_config_at(&test_directory.path().join("koshi.kdl")),
        LoadedAppConfig {
            app_config_outcome: AppConfigOutcome::Absent,
            config_warnings: Vec::new(),
        }
    );
}

#[test]
fn loading_an_app_file_that_cannot_be_read_keeps_the_read_error_with_a_warning() {
    // `koshi.kdl` is a directory: reading it as a string fails on every
    // platform.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    fs::create_dir(&config_file_path).expect("create dir");
    let read_error =
        fs::read_to_string(&config_file_path).expect_err("a directory reads as no string");
    assert_eq!(
        load_app_config_at(&config_file_path),
        LoadedAppConfig {
            app_config_outcome: AppConfigOutcome::Unreadable {
                read_error_text: read_error.to_string(),
            },
            config_warnings: vec![format!(
                "could not read config file {}: {read_error}",
                config_file_path.display()
            )],
        }
    );
}

#[test]
fn an_app_file_with_an_unsupported_version_is_refused_with_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    fs::write(&config_file_path, "version 999\n").expect("write");
    assert_eq!(
        load_app_config_at(&config_file_path),
        LoadedAppConfig {
            app_config_outcome: AppConfigOutcome::Refused {
                app_config_source_text: "version 999\n".to_string(),
            },
            config_warnings: vec![format!(
                "koshi.kdl not applied ({}): line 1, column 1: config schema version 999 is newer \
                 than this koshi supports (2); using defaults",
                config_file_path.display()
            )],
        }
    );
}

#[test]
fn an_app_file_from_a_newer_koshi_is_refused_naming_only_its_version() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    fs::write(&config_file_path, "version 3\nfrobnicate #true\n").expect("write");
    assert_eq!(
        load_app_config_at(&config_file_path),
        LoadedAppConfig {
            app_config_outcome: AppConfigOutcome::Refused {
                app_config_source_text: "version 3\nfrobnicate #true\n".to_string(),
            },
            config_warnings: vec![format!(
                "koshi.kdl not applied ({}): line 1, column 1: config schema version 3 is newer \
                 than this koshi supports (2); using defaults",
                config_file_path.display()
            )],
        }
    );
}

#[test]
fn an_empty_app_file_is_refused_with_a_warning() {
    // An empty file names no `version`, which `parse_app_config` refuses.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    fs::write(&config_file_path, "").expect("write");
    assert_eq!(
        load_app_config_at(&config_file_path),
        LoadedAppConfig {
            app_config_outcome: AppConfigOutcome::Refused {
                app_config_source_text: String::new(),
            },
            config_warnings: vec![format!(
                "koshi.kdl not applied ({}): line 1, column 1: file must declare `version`; using \
                 defaults",
                config_file_path.display()
            )],
        }
    );
}

#[test]
fn an_app_file_with_a_bad_update_value_is_refused_naming_every_problem() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    let app_config_source_text =
        "version 2\nfrobnicate #true\npane {\n}\nupdate { auto-check \"yes\" }\n";
    fs::write(&config_file_path, app_config_source_text).expect("write");
    assert_eq!(
        load_app_config_at(&config_file_path),
        LoadedAppConfig {
            app_config_outcome: AppConfigOutcome::Refused {
                app_config_source_text: app_config_source_text.to_string(),
            },
            config_warnings: vec![format!(
                "koshi.kdl not applied ({}): line 2, column 1: ignored unknown key `frobnicate`; \
                 did you mean `update`?; line 5, column 10: invalid config key \
                 `update.auto-check`: expected a boolean (#true or #false); using defaults",
                config_file_path.display()
            )],
        }
    );
}

#[test]
fn an_app_file_that_is_not_kdl_is_refused_naming_its_path_once() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    fs::write(&config_file_path, "version 2\npane {\n").expect("write");
    assert_eq!(
        load_app_config_at(&config_file_path),
        LoadedAppConfig {
            app_config_outcome: AppConfigOutcome::Refused {
                app_config_source_text: "version 2\npane {\n".to_string(),
            },
            config_warnings: vec![format!(
                "koshi.kdl not applied ({}): line 2, column 6: No closing '}}' for child block; \
                 using defaults",
                config_file_path.display()
            )],
        }
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
    assert_eq!(
        load_app_config_at(&config_file_path),
        LoadedAppConfig {
            app_config_outcome: AppConfigOutcome::Parsed {
                app_config_layer: Box::new(PartialKoshiConfig::default()),
                theme_name: Some("midnight".to_string()),
                app_config_source_text: "version 1\nfrobnicate 1\ntheme \"midnight\"\n".to_string(),
            },
            config_warnings: vec![format!(
                "{}: line 2, column 1: ignored unknown key `frobnicate`; did you mean `update`?",
                config_file_path.display()
            )],
        }
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
            "{}: line 3, column 5: ignored unknown key `colors.foreground`; did you mean \
             `colors.ramp-end`?",
            theme_file_path.display()
        )]
    );
}

#[test]
fn a_theme_file_with_an_unsupported_version_falls_back_to_the_default_with_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let theme_file_path = write_theme_file(test_directory.path(), "midnight", "version 999\n");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "midnight", &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "theme `midnight` not applied ({}): line 1, column 1: config schema version 999 is \
             newer than this koshi supports (2); using the default theme",
            theme_file_path.display()
        )]
    );
}

#[test]
fn a_theme_file_with_two_refusals_names_both_and_falls_back_to_the_default() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let theme_file_path =
        write_theme_file(test_directory.path(), "midnight", "version 0\nversion 2\n");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "midnight", &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "theme `midnight` not applied ({}): line 1, column 1: config schema version must be \
             at least 1; line 2, column 1: `version` is declared more than once; using the \
             default theme",
            theme_file_path.display()
        )]
    );
}

#[test]
fn a_theme_file_that_is_not_kdl_names_its_path_once_and_falls_back_to_the_default() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let theme_file_path =
        write_theme_file(test_directory.path(), "midnight", "version 2\ncolors {\n");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "midnight", &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "theme `midnight` not applied ({}): line 2, column 8: No closing '}}' for child \
             block; using the default theme",
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
    // `keybinding.kdl` is all-or-nothing: any parse error drops the file. The
    // warning names every schema problem with its line and column, joined by
    // `; `.
    let keybinding_source = "mode \"normal\" {\n    bind \"<C-\" \"core:new-tab\"\n}\n";
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("keybinding.kdl");
    fs::write(&config_file_path, keybinding_source).expect("write");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_keybindings_config(&config_file_path, &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "keybinding.kdl not applied ({}): line 1, column 1: keybinding file must \
             declare `version`; line 2, column 10: invalid key `<C-`: missing closing `>`; \
             using defaults",
            config_file_path.display()
        )]
    );
}

#[test]
fn a_keybinding_file_that_is_not_kdl_warns_with_its_line_and_column() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("keybinding.kdl");
    fs::write(&config_file_path, "version 2\nmode \"normal\" {\n").expect("write");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_keybindings_config(&config_file_path, &mut config_warnings),
        None
    );
    assert_eq!(
        config_warnings,
        vec![format!(
            "keybinding.kdl not applied ({}): line 2, column 15: No closing '}}' for child \
             block; using defaults",
            config_file_path.display()
        )]
    );
}

#[test]
fn an_unreadable_keybinding_file_gives_no_layer_with_a_warning() {
    // A directory named `keybinding.kdl` exists and does not read as text.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("keybinding.kdl");
    fs::create_dir(&config_file_path).expect("create directory");
    let read_error =
        fs::read_to_string(&config_file_path).expect_err("a directory does not read as text");
    let mut config_warnings = Vec::new();
    assert_eq!(
        load_keybindings_config(&config_file_path, &mut config_warnings),
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
fn load_keybinding_file_names_why_a_file_gives_no_layer() {
    let test_directory = tempfile::tempdir().expect("temp dir");

    let absent_file_path = test_directory.path().join("absent.kdl");
    match load_keybinding_file(&absent_file_path) {
        Ok(None) => {}
        other_outcome => panic!("expected an absent file, got {other_outcome:?}"),
    }

    let directory_file_path = test_directory.path().join("directory.kdl");
    fs::create_dir(&directory_file_path).expect("create directory");
    let expected_read_error =
        fs::read_to_string(&directory_file_path).expect_err("a directory does not read as text");
    match load_keybinding_file(&directory_file_path) {
        Err(KeybindingFileError::Unreadable(read_error)) => {
            assert_eq!(read_error.kind(), expected_read_error.kind());
            assert_eq!(read_error.to_string(), expected_read_error.to_string());
        }
        other_outcome => panic!("expected an unreadable file, got {other_outcome:?}"),
    }

    let unparseable_file_path = test_directory.path().join("unparseable.kdl");
    fs::write(
        &unparseable_file_path,
        "version 2\nunlock_alternative \"<A-u>\"\n",
    )
    .expect("write");
    match load_keybinding_file(&unparseable_file_path) {
        Err(KeybindingFileError::Unparseable(KeybindingParseError::Invalid {
            keybinding_path,
            diagnostics,
        })) => {
            assert_eq!(keybinding_path, unparseable_file_path.display().to_string());
            let diagnostic_messages: Vec<&str> = diagnostics
                .iter()
                .map(|diagnostic| diagnostic.get_diagnostic_message())
                .collect();
            assert_eq!(
                diagnostic_messages,
                vec!["unknown key `unlock_alternative`; did you mean `unlock-alternative`?"]
            );
        }
        other_outcome => panic!("expected an unparseable file, got {other_outcome:?}"),
    }

    let parsed_file_path = test_directory.path().join("parsed.kdl");
    fs::write(&parsed_file_path, "version 2\nchord-timeout-ms 750\n").expect("write");
    match load_keybinding_file(&parsed_file_path) {
        Ok(Some(keybindings_config_layer)) => assert_eq!(
            keybindings_config_layer,
            PartialKeybindingsConfig {
                chord_timeout_ms: Some(750),
                ..PartialKeybindingsConfig::default()
            }
        ),
        other_outcome => panic!("expected a parsed file, got {other_outcome:?}"),
    }
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

    // Text on disk → `load_app_config_at` → `apply_beta_gate` → a
    // `#[beta_feature]` function.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");

    fs::write(&config_file_path, "version 2\nallow-beta-features #true\n").expect("write");
    let loaded_app_config = load_app_config_at(&config_file_path);
    assert_eq!(loaded_app_config.config_warnings, Vec::<String>::new());
    apply_beta_gate(loaded_app_config.into_app_config_layer());
    assert_eq!(mock_beta_entry_point(), 1);

    fs::write(&config_file_path, "version 2\nallow-beta-features #false\n").expect("write");
    let loaded_app_config = load_app_config_at(&config_file_path);
    assert_eq!(loaded_app_config.config_warnings, Vec::<String>::new());
    apply_beta_gate(loaded_app_config.into_app_config_layer());
    assert_eq!(mock_beta_entry_point(), 0);
}

#[test]
fn build_logging_parameters_with_no_config_file_are_the_defaults() {
    let logging_parameters = build_logging_parameters(None, LogFileOwner::Router);

    assert!(!logging_parameters.is_enabled);
    assert_eq!(logging_parameters.log_level, LogLevel::Warning);
    assert_eq!(logging_parameters.log_format, LogFormat::Pretty);
    assert_eq!(logging_parameters.log_file_owner, LogFileOwner::Router);
    assert_eq!(logging_parameters.max_log_file_size_bytes, 4_194_304);
}

#[test]
fn build_logging_parameters_take_the_level_format_and_size_the_config_names() {
    let session_id = SessionId::new();
    let app_config = PartialKoshiConfig {
        logging: Some(PartialLoggingConfig {
            is_enabled: Some(true),
            log_level: Some(LogLevel::Info),
            log_format: Some(LogFormat::Json),
            max_log_file_size_in_mb: Some(16),
        }),
        ..Default::default()
    };

    let logging_parameters =
        build_logging_parameters(Some(&app_config), LogFileOwner::Session { session_id });

    assert!(logging_parameters.is_enabled);
    assert_eq!(logging_parameters.log_level, LogLevel::Info);
    assert_eq!(logging_parameters.log_format, LogFormat::Json);
    assert_eq!(
        logging_parameters.log_file_owner,
        LogFileOwner::Session { session_id }
    );
    assert_eq!(logging_parameters.max_log_file_size_bytes, 16_777_216);
}

#[test]
fn build_logging_parameters_keep_the_defaults_for_every_field_the_config_leaves_out() {
    let session_id = SessionId::new();
    let app_config = PartialKoshiConfig {
        logging: Some(PartialLoggingConfig {
            is_enabled: Some(true),
            log_level: None,
            log_format: None,
            max_log_file_size_in_mb: None,
        }),
        ..Default::default()
    };

    let logging_parameters =
        build_logging_parameters(Some(&app_config), LogFileOwner::Session { session_id });

    assert!(logging_parameters.is_enabled);
    assert_eq!(logging_parameters.log_level, LogLevel::Warning);
    assert_eq!(logging_parameters.log_format, LogFormat::Pretty);
    assert_eq!(
        logging_parameters.log_file_owner,
        LogFileOwner::Session { session_id }
    );
    assert_eq!(logging_parameters.max_log_file_size_bytes, 4_194_304);
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

#[cfg(unix)]
/// A `koshi.kdl` layer with `should_allow_other_users` and no shared directory.
fn build_other_user_access_config_layer(should_allow_other_users: bool) -> PartialKoshiConfig {
    PartialKoshiConfig {
        should_allow_other_users: Some(should_allow_other_users),
        ..Default::default()
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
/// The directory `other_users_policy` shares through, or `None` when the
/// session serves only the user who started it.
fn get_shared_sessions_directory(other_users_policy: Option<OtherUsers>) -> Option<PathBuf> {
    other_users_policy.map(|other_users_access_policy| other_users_access_policy.shared_directory)
}

#[cfg(unix)]
#[test]
fn a_fresh_install_serves_only_the_user_who_started_the_session() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(None, false, None)),
        None
    );
}

#[cfg(unix)]
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

#[cfg(unix)]
#[test]
fn a_config_turning_the_switch_on_shares_through_the_machine_wide_directory() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(
            Some(&build_other_user_access_config_layer(true)),
            false,
            None,
        )),
        Some(PathBuf::from("/tmp/koshi"))
    );
}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
#[test]
fn the_flag_shares_a_session_that_has_no_config_file_at_all() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(None, true, None)),
        Some(PathBuf::from("/tmp/koshi"))
    );
}

// --- The settings a viewer resolves before it dials ---

#[test]
fn resolve_client_config_with_no_app_file_gives_the_defaults() {
    assert_eq!(resolve_client_config(None), ClientConfig::default());
}

#[test]
fn resolve_client_config_takes_the_values_the_app_file_names() {
    let app_config_layer = PartialKoshiConfig {
        supports_image_protocols: Some(false),
        remote_link_timeout_ms: Some(45_000),
        ..Default::default()
    };
    assert_eq!(
        resolve_client_config(Some(app_config_layer)),
        ClientConfig {
            supports_image_protocols: false,
            remote_link_timeout_ms: 45_000,
            ..ClientConfig::default()
        }
    );

    assert_eq!(
        resolve_client_config(Some(PartialKoshiConfig::default())),
        ClientConfig::default()
    );
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
    let (loaded_config, config_warnings) =
        load_config_files_on_app_config(None, load_app_config_file(None));

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

#[cfg(unix)]
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

#[cfg(windows)]
#[test]
fn a_config_directory_turning_the_switch_on_names_no_shared_directory_on_windows() {
    let config_directory = build_config_directory_with_app_config(
        "version 2\nallow-other-users #true\nshared-sessions-dir \"C:\\\\koshi-shared\"\n",
    );

    assert_eq!(
        find_shared_sessions_base_directory(Some(config_directory.path())),
        None
    );
}

#[test]
fn a_config_turning_the_switch_on_loads_with_no_warning() {
    let config_directory = build_config_directory_with_app_config(
        "version 2\nallow-other-users #true\nshared-sessions-dir \"/var/run/koshi\"\n",
    );

    let (loaded_config, config_warnings) = load_config_files_on_app_config(
        Some(config_directory.path()),
        load_app_config_file(Some(config_directory.path())),
    );

    assert_eq!(config_warnings, Vec::<String>::new());
    let app_config_layer = loaded_config.app_config_layer.expect("koshi.kdl applies");
    assert_eq!(
        (
            app_config_layer.should_allow_other_users,
            app_config_layer.shared_sessions_directory
        ),
        (Some(true), Some(Some(PathBuf::from("/var/run/koshi"))))
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

#[cfg(unix)]
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

    fs::write(
        config_directory.path().join("koshi.kdl"),
        "version 2\nallow-other-users #true\nshared-sessions-dir \"/var/run/koshi\"\n",
    )
    .expect("rewrite koshi.kdl");
    assert!((other_users_policy.is_enabled)());

    let unparseable_config_text = "version 2\nallow-other-users #true {\n";
    parse_app_config(
        &config_directory.path().join("koshi.kdl"),
        unparseable_config_text,
    )
    .expect_err("an unclosed block does not parse");
    fs::write(
        config_directory.path().join("koshi.kdl"),
        unparseable_config_text,
    )
    .expect("rewrite koshi.kdl");
    assert!(
        !(other_users_policy.is_enabled)(),
        "a file that does not parse turns the switch off"
    );

    fs::remove_file(config_directory.path().join("koshi.kdl")).expect("remove koshi.kdl");
    assert!(
        !(other_users_policy.is_enabled)(),
        "an absent file turns the switch off"
    );
}

#[test]
fn a_server_config_file_gives_the_remote_listen_address_it_names() {
    let config_directory = tempfile::tempdir().expect("a config directory");
    let app_config_path = config_directory.path().join("koshi.kdl");
    fs::write(
        &app_config_path,
        "version 2\nremote-listen \"127.0.0.1:7654\"\n",
    )
    .expect("the koshi.kdl is written");

    assert_eq!(
        load_server_config_file(&app_config_path).remote_listen_address,
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], 7654)))
    );
}

#[test]
fn an_absent_or_unparseable_server_config_file_gives_the_built_in_defaults() {
    let config_directory = tempfile::tempdir().expect("a config directory");
    let app_config_path = config_directory.path().join("koshi.kdl");

    assert_eq!(
        load_server_config_file(&app_config_path),
        ServerConfig::default(),
        "an absent file"
    );

    fs::write(&app_config_path, "version 2\nremote-listen {\n").expect("the koshi.kdl is written");
    assert_eq!(
        load_server_config_file(&app_config_path),
        ServerConfig::default(),
        "a file that does not parse"
    );
}

#[test]
fn a_server_config_resolves_from_the_layer_folded_onto_the_built_in_defaults() {
    let app_config_layer = PartialKoshiConfig {
        remote_listen_address: Some(Some(std::net::SocketAddr::from(([127, 0, 0, 1], 7654)))),
        ..PartialKoshiConfig::default()
    };

    assert_eq!(
        resolve_server_config(Some(&app_config_layer)),
        ServerConfig {
            remote_listen_address: Some(std::net::SocketAddr::from(([127, 0, 0, 1], 7654))),
            ..ServerConfig::default()
        },
        "a layer naming an address"
    );
    assert_eq!(
        resolve_server_config(None),
        ServerConfig::default(),
        "no layer"
    );
}

#[test]
fn parsing_koshi_kdl_text_gives_what_reading_the_file_gives() {
    let app_config_text = "version 2\ntheme \"midnight\"\nfrobnicate 1\n";
    let config_directory = build_config_directory_with_app_config(app_config_text);
    let app_config_path = config_directory.path().join("koshi.kdl");

    let loaded_app_config = load_app_config_file(Some(config_directory.path()));

    assert_eq!(
        loaded_app_config,
        LoadedAppConfig {
            app_config_outcome: AppConfigOutcome::Parsed {
                app_config_layer: Box::new(PartialKoshiConfig::default()),
                theme_name: Some("midnight".to_string()),
                app_config_source_text: app_config_text.to_string(),
            },
            config_warnings: vec![format!(
                "{}: line 3, column 1: ignored unknown key `frobnicate`; did you mean `update`?",
                app_config_path.display()
            )],
        }
    );
    assert_eq!(
        parse_app_config_text(&app_config_path, app_config_text.to_string()),
        loaded_app_config
    );
}

#[test]
fn config_files_build_on_the_koshi_kdl_already_read_and_read_its_theme() {
    // `koshi.kdl` on disk names the theme `disk` and leaves `allow-other-users`
    // off. The `koshi.kdl` already read names the theme `passed` and turns it
    // on. Only the one already read applies.
    let config_directory = build_config_directory_with_app_config("version 2\ntheme \"disk\"\n");
    write_theme_file(
        config_directory.path(),
        "disk",
        "version 1\ncolors {\n    accent \"#000000\"\n}\n",
    );
    write_theme_file(
        config_directory.path(),
        "passed",
        "version 1\ncolors {\n    accent \"#f5c2ff\"\n}\n",
    );
    let loaded_app_config = LoadedAppConfig {
        app_config_outcome: AppConfigOutcome::Parsed {
            app_config_layer: Box::new(PartialKoshiConfig {
                should_allow_other_users: Some(true),
                ..PartialKoshiConfig::default()
            }),
            theme_name: Some("passed".to_string()),
            app_config_source_text: "version 2\ntheme \"passed\"\nallow-other-users #true\n"
                .to_string(),
        },
        config_warnings: vec!["koshi.kdl: line 3, column 1: a warning of the read".to_string()],
    };

    let (loaded_config, config_warnings) =
        load_config_files_on_app_config(Some(config_directory.path()), loaded_app_config);

    assert_eq!(
        loaded_config.app_config_layer,
        Some(PartialKoshiConfig {
            should_allow_other_users: Some(true),
            ..PartialKoshiConfig::default()
        })
    );
    let theme_config_layer = loaded_config
        .theme_config_layer
        .expect("the passed theme loads");
    assert_eq!(theme_config_layer.theme_name, Some("passed".to_string()));
    assert_eq!(
        theme_config_layer.colors.expect("colors set").accent,
        Some(RgbColor {
            red: 0xf5,
            green: 0xc2,
            blue: 0xff
        })
    );
    assert_eq!(loaded_config.keybindings_config_layer, None);
    assert_eq!(
        config_warnings,
        vec!["koshi.kdl: line 3, column 1: a warning of the read".to_string()]
    );
}

#[cfg(unix)]
#[test]
fn a_layer_turning_the_switch_on_names_its_shared_directory() {
    let app_config_layer = PartialKoshiConfig {
        should_allow_other_users: Some(true),
        shared_sessions_directory: Some(Some(PathBuf::from("/var/run/koshi"))),
        ..PartialKoshiConfig::default()
    };

    assert_eq!(
        resolve_shared_sessions_base_directory_for_layer(Some(&app_config_layer)),
        Some(PathBuf::from("/var/run/koshi"))
    );
    assert_eq!(resolve_shared_sessions_base_directory_for_layer(None), None);
}
