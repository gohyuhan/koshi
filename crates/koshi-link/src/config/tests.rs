//! Tests for config file loading — name validation for the name-selected
//! files (profiles, themes) and the per-file readers that take an explicit
//! path.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use koshi_beta::beta_feature;
use koshi_config::layer::PartialLoggingConfig;
use koshi_config::types::{BoundAction, ModeBindings, ModeName, RgbColor};
use koshi_core::action::ActionReference;
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::log::{LogFormat, LogLevel};
use koshi_core::resolve::ActionArgs;
use tempfile::TempDir;

use super::*;

/// Stands in for a real beta-gated entry point: the gate decides whether the
/// body runs, and the two answers differ, so a closed gate is visible.
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
    // Each of these would join to a `.kdl` outside `profile/` or `themes/`.
    assert!(!is_plain_file_name("../secret"));
    assert!(!is_plain_file_name("a/b"));
    assert!(!is_plain_file_name("/etc/passwd"));
    assert!(!is_plain_file_name(".."));
    assert!(!is_plain_file_name("."));
    assert!(!is_plain_file_name(""));
}

#[test]
fn a_nested_or_trailing_separator_name_is_rejected() {
    // `foo/` would read `profile/foo/.kdl` — a nested file, not the flat
    // `profile/<name>.kdl` the rule requires; `foo/..` walks back out.
    assert!(!is_plain_file_name("foo/"));
    assert!(!is_plain_file_name("foo/.."));
}

#[test]
fn a_leading_or_embedded_dot_name_stays_plain() {
    // Only the exact `.` and `..` components are rejected; a leading dot or a
    // double dot inside a longer name is an ordinary flat file name.
    assert!(is_plain_file_name(".hidden"));
    assert!(is_plain_file_name("a..b"));
    assert!(is_plain_file_name("..config"));
    assert!(is_plain_file_name("config.."));
}

#[test]
fn a_space_or_non_ascii_name_stays_plain() {
    // Neither is a path separator on any platform, so each is one flat file
    // name that joins to a `.kdl` directly under `profile/` or `themes/`.
    assert!(is_plain_file_name(" "));
    assert!(is_plain_file_name("my profile"));
    assert!(is_plain_file_name("日本語"));
    assert!(is_plain_file_name("thème"));
}

#[test]
fn a_backslash_in_a_name_follows_the_platform_separator() {
    // A backslash is a path separator on Windows (so `a\b` names a nested
    // file and is rejected) but an ordinary character on Unix (so `a\b` is a
    // single flat file name and stays plain).
    #[cfg(windows)]
    assert!(!is_plain_file_name("a\\b"));
    #[cfg(not(windows))]
    assert!(is_plain_file_name("a\\b"));
}

// --- load_config_file: absent, present, and unreadable files ---

#[test]
fn reading_an_absent_file_is_none_without_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(
        load_config_file(&test_directory.path().join("missing.kdl"), &mut warnings),
        None
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn reading_a_present_file_returns_its_exact_text() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("present.kdl");
    std::fs::write(&config_file_path, "version 1\n").expect("write");
    let mut warnings = Vec::new();
    assert_eq!(
        load_config_file(&config_file_path, &mut warnings),
        Some("version 1\n".to_string())
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn reading_a_directory_as_a_file_warns_and_is_none() {
    // A path that exists but is a directory is readable-as-a-string nowhere,
    // so `load_config_file` takes its error arm on every platform.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(load_config_file(test_directory.path(), &mut warnings), None);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "could not read config file {}: ",
            test_directory.path().display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
}

#[test]
fn reading_an_empty_file_returns_an_empty_string_without_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("empty.kdl");
    std::fs::write(&config_file_path, "").expect("write");
    let mut warnings = Vec::new();
    assert_eq!(
        load_config_file(&config_file_path, &mut warnings),
        Some(String::new())
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn reading_a_file_that_is_not_utf8_warns_and_is_none() {
    // `0x80` starts no UTF-8 sequence, so the read fails on every platform.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("invalid.kdl");
    std::fs::write(&config_file_path, [0x76, 0x65, 0x80, 0x72]).expect("write");
    let mut warnings = Vec::new();
    assert_eq!(load_config_file(&config_file_path, &mut warnings), None);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "could not read config file {}: ",
            config_file_path.display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
}

// --- load_app_config: clean, field-warning-free, and hard-error files ---

#[test]
fn loading_a_clean_app_file_returns_a_layer_without_warnings() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    std::fs::write(&config_file_path, "version 1\n").expect("write");
    let mut warnings = Vec::new();
    let app_config_file =
        load_app_config(&config_file_path, &mut warnings).expect("the file loads");
    assert_eq!(app_config_file.layer, PartialKoshiConfig::default());
    assert_eq!(app_config_file.theme_name, None);
    assert_eq!(app_config_file.parse_warnings, Vec::<String>::new());
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn loading_an_absent_app_file_is_none_without_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(
        load_app_config(&test_directory.path().join("koshi.kdl"), &mut warnings),
        None
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn an_app_file_with_an_unsupported_version_drops_to_defaults_with_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    std::fs::write(&config_file_path, "version 999\n").expect("write");
    let mut warnings = Vec::new();
    assert_eq!(load_app_config(&config_file_path, &mut warnings), None);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "koshi.kdl not applied ({}): ",
            config_file_path.display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
    assert!(
        warnings[0].ends_with("; using defaults"),
        "unexpected warning: {}",
        warnings[0]
    );
}

#[test]
fn an_empty_app_file_drops_to_defaults_with_a_warning() {
    // An empty file names no `version`, which `parse_app_config` refuses.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    std::fs::write(&config_file_path, "").expect("write");
    let mut warnings = Vec::new();
    assert_eq!(load_app_config(&config_file_path, &mut warnings), None);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "koshi.kdl not applied ({}): ",
            config_file_path.display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
    assert!(
        warnings[0].ends_with("; using defaults"),
        "unexpected warning: {}",
        warnings[0]
    );
}

#[test]
fn an_unknown_app_field_is_kept_as_a_path_prefixed_skip_warning() {
    // A `koshi.kdl` that parses but names an unknown key applies its other
    // fields and records the skip, prefixed with the file it came from.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("koshi.kdl");
    std::fs::write(
        &config_file_path,
        "version 1\nfrobnicate 1\ntheme \"midnight\"\n",
    )
    .expect("write");
    let mut warnings = Vec::new();
    let app_config_file =
        load_app_config(&config_file_path, &mut warnings).expect("the file loads");
    assert_eq!(app_config_file.theme_name, Some("midnight".to_string()));
    assert_eq!(
        warnings,
        vec![format!(
            "{}: ignored unknown key `frobnicate`; did you mean `update`?",
            config_file_path.display()
        )]
    );
}

// --- load_theme: selecting a `themes/<name>.kdl` by name ---

/// Writes `theme_source` to `themes/<theme_name>.kdl` under `config_directory`,
/// creates the theme directory, and returns the theme file path.
fn write_theme(config_directory: &Path, theme_name: &str, theme_source: &str) -> PathBuf {
    let themes_directory = config_directory.join("themes");
    std::fs::create_dir_all(&themes_directory).expect("create themes dir");
    let theme_file_path = themes_directory.join(format!("{theme_name}.kdl"));
    std::fs::write(&theme_file_path, theme_source).expect("write");
    theme_file_path
}

#[test]
fn a_selected_theme_is_read_from_the_themes_directory_and_named_after_its_file() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    write_theme(
        test_directory.path(),
        "midnight",
        "version 1\ncolors {\n    accent \"#f5c2ff\"\n}\n",
    );
    let mut warnings = Vec::new();
    let layer =
        load_theme_config(test_directory.path(), "midnight", &mut warnings).expect("theme loads");
    assert_eq!(layer.theme_name, Some("midnight".to_string()));
    assert_eq!(
        layer.colors.expect("colors set").accent,
        Some(RgbColor {
            red: 0xf5,
            green: 0xc2,
            blue: 0xff
        })
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn selecting_the_default_theme_by_name_keeps_the_built_in_colors_silently() {
    // `default` is the built-in theme, so it is never looked up on disk — and
    // asking for it is a normal choice, not a problem to warn about.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), DEFAULT_THEME, &mut warnings),
        None
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn a_default_named_theme_file_is_ignored_in_favor_of_the_built_in_theme() {
    // Even with a `themes/default.kdl` on disk, the reserved name means the
    // built-in colors: the file is never read.
    let test_directory = tempfile::tempdir().expect("temp dir");
    write_theme(
        test_directory.path(),
        DEFAULT_THEME,
        "colors {\n    accent \"#ff0000\"\n}\n",
    );
    let mut warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), DEFAULT_THEME, &mut warnings),
        None
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn a_theme_with_no_file_falls_back_to_the_default_with_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "missing", &mut warnings),
        None
    );
    assert_eq!(
        warnings,
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
    // `theme "../../secret"` must not reach a `.kdl` outside `themes/`.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "../../secret", &mut warnings),
        None
    );
    assert_eq!(
        warnings,
        vec!["theme name `../../secret` must be a plain name; using the default theme".to_string()]
    );
}

#[test]
fn an_empty_theme_name_is_rejected_before_any_file_is_read() {
    // `""` has no file name at all, so it never joins to a path under
    // `themes/`.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "", &mut warnings),
        None
    );
    assert_eq!(
        warnings,
        vec!["theme name `` must be a plain name; using the default theme".to_string()]
    );
}

#[test]
fn an_unknown_theme_field_is_kept_as_a_path_prefixed_skip_warning() {
    // A theme file that parses but names an unknown color role applies its other
    // fields and records the skip, prefixed with the file it came from.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = write_theme(
        test_directory.path(),
        "midnight",
        "version 1\ncolors {\n    foreground \"#ffffff\"\n}\n",
    );
    let mut warnings = Vec::new();
    let layer = load_theme_config(test_directory.path(), "midnight", &mut warnings)
        .expect("the theme loads");
    assert_eq!(layer.theme_name, Some("midnight".to_string()));
    assert_eq!(
        warnings,
        vec![format!(
            "{}: ignored unknown key `colors.foreground`; did you mean `colors.ramp-end`?",
            config_file_path.display()
        )]
    );
}

#[test]
fn a_theme_file_with_an_unsupported_version_falls_back_to_the_default_with_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = write_theme(test_directory.path(), "midnight", "version 999\n");
    let mut warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "midnight", &mut warnings),
        None
    );
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "theme `midnight` not applied ({}): ",
            config_file_path.display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
    assert!(
        warnings[0].ends_with("; using the default theme"),
        "unexpected warning: {}",
        warnings[0]
    );
}

#[test]
fn an_unreadable_theme_file_reports_the_cause_and_the_fallback_in_one_line() {
    // A directory named `midnight.kdl` exists but reads as no string on any
    // platform, so the read fails with something other than `NotFound` and the
    // built-in theme stands. One warning carries the path, the OS reason, and
    // what koshi used instead.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("themes").join("midnight.kdl");
    std::fs::create_dir_all(&config_file_path).expect("create dir in place of the file");
    let mut warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "midnight", &mut warnings),
        None
    );
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "theme `midnight` could not be read ({}): ",
            config_file_path.display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
    assert!(
        warnings[0].ends_with("; using the default theme"),
        "unexpected warning: {}",
        warnings[0]
    );
}

#[test]
fn a_missing_theme_is_reported_as_missing_not_as_unreadable() {
    // The absent case and the unreadable case are told apart by the error kind
    // off a single read, so each warning names the real cause: a theme that was
    // never there says "not found", never "could not be read".
    let test_directory = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(test_directory.path().join("themes")).expect("create themes dir");
    let mut warnings = Vec::new();
    assert_eq!(
        load_theme_config(test_directory.path(), "midnight", &mut warnings),
        None
    );
    assert_eq!(
        warnings,
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

#[test]
fn every_theme_failure_says_which_theme_stands_instead() {
    // One assertion over all four failure paths: whatever went wrong, the last
    // thing the user reads is what koshi actually drew with.
    let test_directory = tempfile::tempdir().expect("temp dir");
    write_theme(test_directory.path(), "broken", "version 999\n");
    let unreadable = test_directory.path().join("themes").join("unreadable.kdl");
    std::fs::create_dir_all(&unreadable).expect("create dir in place of the file");

    for theme_name in ["../../secret", "missing", "unreadable", "broken"] {
        let mut warnings = Vec::new();
        assert_eq!(
            load_theme_config(test_directory.path(), theme_name, &mut warnings),
            None
        );
        let last_warning = warnings.last().expect("a warning per failure");
        assert!(
            last_warning.ends_with("; using the default theme"),
            "`{theme_name}` failed without naming the fallback: {last_warning}"
        );
    }
}

// --- load_keybindings_config: valid and unparseable files ---

#[test]
fn loading_a_valid_keybinding_file_returns_a_layer_without_warnings() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("keybinding.kdl");
    std::fs::write(
        &config_file_path,
        "version 1\nmode \"normal\" {\n    bind \"<C-y>\" \"core:new-tab\"\n}\n",
    )
    .expect("write");
    let mut warnings = Vec::new();
    let layer = load_keybindings_config(&config_file_path, &mut warnings).expect("the file loads");
    assert_eq!(layer.chord_timeout_ms, None);
    assert_eq!(layer.which_key_delay_ms, None);
    assert_eq!(layer.max_chord_depth, None);
    assert_eq!(layer.leader, None);
    assert_eq!(layer.unlock_alternative, None);
    assert_eq!(
        layer.mode_bindings_by_name,
        Some(BTreeMap::from([(
            ModeName::from_text("normal"),
            ModeBindings {
                bound_action_by_key_sequence: BTreeMap::from([(
                    KeySequence::from(KeyChord::from_parts(ModFlags::CTRL, Key::Char('y'))),
                    BoundAction {
                        action_reference: ActionReference::from_core_action_name("new-tab")
                            .expect("a core action name"),
                        action_arguments: ActionArgs::None,
                    },
                )]),
                removed_key_sequences: BTreeSet::new(),
            },
        )]))
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn loading_an_absent_keybinding_file_is_none_without_a_warning() {
    let test_directory = tempfile::tempdir().expect("temp dir");
    let mut warnings = Vec::new();
    assert_eq!(
        load_keybindings_config(&test_directory.path().join("keybinding.kdl"), &mut warnings),
        None
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn an_unparseable_keybinding_file_drops_the_whole_file_with_a_warning() {
    // `keybinding.kdl` is all-or-nothing: any parse error drops the file.
    let test_directory = tempfile::tempdir().expect("temp dir");
    let config_file_path = test_directory.path().join("keybinding.kdl");
    std::fs::write(
        &config_file_path,
        "mode \"normal\" {\n    bind \"<C-\" \"core:new-tab\"\n}\n",
    )
    .expect("write");
    let mut warnings = Vec::new();
    assert_eq!(
        load_keybindings_config(&config_file_path, &mut warnings),
        None
    );
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "keybinding.kdl not applied ({}): ",
            config_file_path.display()
        )),
        "unexpected warning: {}",
        warnings[0]
    );
    assert!(
        warnings[0].ends_with("; using defaults"),
        "unexpected warning: {}",
        warnings[0]
    );
}

// --- append_config_field_warnings: file-prefixed skip lines ---

#[test]
fn append_config_field_warnings_prefixes_each_skip_with_the_file_path() {
    let config_file_path = Path::new("some/koshi.kdl");
    let mut warnings = vec!["earlier".to_string()];
    append_config_field_warnings(
        config_file_path,
        &["first skip".to_string(), "second skip".to_string()],
        &mut warnings,
    );
    assert_eq!(
        warnings,
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
    let mut warnings = Vec::new();
    append_config_field_warnings(config_file_path, &[], &mut warnings);
    assert_eq!(warnings, Vec::<String>::new());
}

/// The startup wiring: the `koshi.kdl` knob reaches the process-wide gate.
/// One test walks both answers, because the gate is one flag and separate
/// tests would race each other over it.
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
    assert!(koshi_beta::are_beta_features_allowed());

    apply_beta_gate(Some(disabled_config));
    assert!(!koshi_beta::are_beta_features_allowed());

    // No `koshi.kdl` at all closes an open gate.
    apply_beta_gate(Some(enabled_config));
    assert!(koshi_beta::are_beta_features_allowed());
    apply_beta_gate(None);
    assert!(!koshi_beta::are_beta_features_allowed());

    // The whole chain from text on disk: the reader `load_app_layer` uses, onto
    // the gate, into a function carrying the attribute. `load_app_layer` takes
    // its directory from the platform, so the file goes to `load_app` here.
    let test_directory = TempDir::new().unwrap();
    let config_file_path = test_directory.path().join("koshi.kdl");

    fs::write(&config_file_path, "version 1\nallow-beta-features #true\n").unwrap();
    let mut warnings = Vec::new();
    apply_beta_gate(
        load_app_config(&config_file_path, &mut warnings)
            .map(|app_config_file| app_config_file.layer),
    );
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(mock_beta_entry_point(), 1);

    fs::write(&config_file_path, "version 1\nallow-beta-features #false\n").unwrap();
    let mut warnings = Vec::new();
    apply_beta_gate(
        load_app_config(&config_file_path, &mut warnings)
            .map(|app_config_file| app_config_file.layer),
    );
    assert_eq!(warnings, Vec::<String>::new());
    assert_eq!(mock_beta_entry_point(), 0);
}

#[test]
fn build_logging_params_with_no_config_file_are_the_defaults() {
    let session_id = SessionId::new();
    let logging_params = build_logging_params(None, session_id);

    assert!(!logging_params.is_enabled);
    assert_eq!(logging_params.log_level, LogLevel::Warning);
    assert_eq!(logging_params.log_format, LogFormat::Pretty);
    assert_eq!(logging_params.session_id, session_id);
}

#[test]
fn build_logging_params_take_the_level_and_format_the_config_names() {
    let session_id = SessionId::new();
    let app_config = PartialKoshiConfig {
        logging: Some(PartialLoggingConfig {
            is_enabled: Some(true),
            level: Some(LogLevel::Info),
            log_format: Some(LogFormat::Json),
        }),
        ..Default::default()
    };

    let logging_params = build_logging_params(Some(&app_config), session_id);

    assert!(logging_params.is_enabled);
    assert_eq!(logging_params.log_level, LogLevel::Info);
    assert_eq!(logging_params.log_format, LogFormat::Json);
    assert_eq!(logging_params.session_id, session_id);
}

#[test]
fn build_logging_params_keep_the_defaults_for_every_field_the_config_leaves_out() {
    let session_id = SessionId::new();
    let app_config = PartialKoshiConfig {
        logging: Some(PartialLoggingConfig {
            is_enabled: Some(true),
            level: None,
            log_format: None,
        }),
        ..Default::default()
    };

    let logging_params = build_logging_params(Some(&app_config), session_id);

    assert!(logging_params.is_enabled);
    assert_eq!(logging_params.log_level, LogLevel::Warning);
    assert_eq!(logging_params.log_format, LogFormat::Pretty);
    assert_eq!(logging_params.session_id, session_id);
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

/// A `koshi.kdl` layer setting `allow-other-users` to `is_allowed` and naming no
/// shared directory.
fn build_access_layer(is_allowed: bool) -> PartialKoshiConfig {
    PartialKoshiConfig {
        should_allow_other_users: Some(is_allowed),
        ..Default::default()
    }
}

/// A `koshi.kdl` layer setting `allow-other-users` to `is_allowed` and naming
/// `shared_directory` as the shared sessions directory.
fn build_access_layer_with_shared_directory(
    is_allowed: bool,
    shared_directory: &str,
) -> PartialKoshiConfig {
    PartialKoshiConfig {
        should_allow_other_users: Some(is_allowed),
        shared_sessions_directory: Some(Some(PathBuf::from(shared_directory))),
        ..Default::default()
    }
}

/// The directory a policy shares through, or `None` when the session serves
/// only the user who started it. `OtherUsers` carries a closure, so the
/// directory is what a test compares.
fn get_shared_sessions_directory(policy: Option<OtherUsers>) -> Option<PathBuf> {
    policy.map(|policy| policy.shared_directory)
}

#[test]
fn a_fresh_install_serves_only_the_user_who_started_the_session() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(None, None)),
        None
    );
}

#[test]
fn a_config_turning_the_switch_off_serves_only_that_user() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(
            Some(&build_access_layer(false)),
            None,
        )),
        None
    );
}

#[test]
fn a_config_turning_the_switch_on_shares_through_the_machine_wide_directory() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(
            Some(&build_access_layer(true)),
            None,
        )),
        koshi_paths::resolve_shared_sessions_directory()
    );
}

#[test]
fn a_config_naming_a_shared_directory_shares_through_that_one() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(
            Some(&build_access_layer_with_shared_directory(
                true,
                "/var/run/koshi"
            )),
            None
        )),
        Some(PathBuf::from("/var/run/koshi"))
    );
}

#[test]
fn naming_a_shared_directory_alone_serves_only_this_user() {
    // The directory says where the sockets would go, never who may reach them.
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(
            Some(&build_access_layer_with_shared_directory(
                false,
                "/var/run/koshi"
            )),
            None
        )),
        None
    );
}

#[test]
fn the_flag_shares_a_session_whose_config_says_no() {
    let policy = resolve_other_users_policy(
        Some(&build_access_layer_with_shared_directory(
            false,
            "/var/run/koshi",
        )),
        Some(true),
    )
    .expect("the flag turns the switch on");

    assert_eq!(policy.shared_directory, PathBuf::from("/var/run/koshi"));
    // A service unit started under the flag keeps serving whatever the app file
    // says afterwards, so the live read answers the same every time.
    assert!((policy.is_enabled)());
    assert!((policy.is_enabled)());
}

#[test]
fn the_flag_shares_a_session_that_has_no_config_file_at_all() {
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(None, Some(true))),
        koshi_paths::resolve_shared_sessions_directory()
    );
}

#[test]
fn a_flag_naming_no_other_users_serves_only_this_user() {
    // `--allow-other-users` sends `Some(true)` or nothing, so no command line
    // spells this today. An explicit answer beats the app file either way.
    assert_eq!(
        get_shared_sessions_directory(resolve_other_users_policy(
            Some(&build_access_layer_with_shared_directory(
                true,
                "/var/run/koshi"
            )),
            Some(false)
        )),
        None
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
