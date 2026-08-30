//! Tests for the `koshi.kdl` app-config parser.

use std::path::{Path, PathBuf};

use kdl::KdlDocument;
use koshi_core::geometry::Direction;
use koshi_core::log::{LogFormat, LogLevel};

use crate::error::ConfigError;
use crate::layer::{
    PartialKoshiConfig, PartialPaneConfig, PartialScrollbackConfig, PartialUpdateConfig,
};
use crate::types::{ClientConfig, WheelScroll};

use super::{parse_app_config, AppConfigFile};

/// Parses `source` as `koshi.kdl`, panicking on error, dropping warnings.
fn parse(source: &str) -> PartialKoshiConfig {
    parse_file(source).layer
}

/// Parses `source`, returning both the layer and the warnings.
fn parse_with_warnings(source: &str) -> (PartialKoshiConfig, Vec<String>) {
    let file = parse_file(source);
    (file.layer, file.warnings)
}

/// Parses `source` as `koshi.kdl` whole — layer, theme name, and warnings —
/// panicking on error.
fn parse_file(source: &str) -> AppConfigFile {
    let source = current_source(source);
    parse_app_config(Path::new("koshi.kdl"), &source).expect("valid config")
}

/// The message a [`ConfigError::Parse`] carries for `source`: the first
/// sub-diagnostic of the raw kdl parse error.
fn kdl_first_diagnostic(source: &str) -> String {
    source
        .parse::<KdlDocument>()
        .expect_err("source is invalid KDL")
        .diagnostics
        .first()
        .expect("kdl reports a diagnostic")
        .to_string()
}

/// Asserts `error` is [`ConfigError::Validation`] with exactly this `key` and
/// `detail`.
#[track_caller]
fn assert_validation(error: ConfigError, key: &str, detail: &str) {
    let ConfigError::Validation {
        key: got_key,
        detail: got_detail,
    } = error
    else {
        panic!("expected a validation error, got {error:?}");
    };
    assert_eq!(got_key, key);
    assert_eq!(got_detail, detail);
}

fn current_source(source: &str) -> String {
    if source
        .lines()
        .any(|line| line.trim_start().starts_with("version "))
    {
        source.to_string()
    } else if let Some(source) = source.strip_prefix('\u{feff}') {
        format!("\u{feff}version 1\n{source}")
    } else {
        format!("version 1\n{source}")
    }
}

#[test]
fn version_only_source_sets_no_layer() {
    let file = parse_file("");
    assert_eq!(file, AppConfigFile::default());
}

#[test]
fn missing_version_is_a_validation_error() {
    let error =
        parse_app_config(Path::new("koshi.kdl"), "pane {}").expect_err("version is required");
    assert_validation(error, "version", "file must declare `version`");
}

#[test]
fn the_theme_line_records_the_name_outside_the_merge_layer() {
    // `koshi.kdl` only names the theme; the colors come from the matching
    // `themes/<name>.kdl`, which the loader reads. The name rides beside the
    // layer, never inside it.
    let file = parse_file("theme \"midnight\"");
    assert_eq!(file.theme, Some("midnight".to_string()));
    assert_eq!(file.layer.theme, None);
}

#[test]
fn a_blank_theme_name_is_skipped_with_a_warning() {
    // An empty name is skipped like any other bad field; the built-in theme
    // stands.
    let file = parse_file("theme \"\"");
    assert_eq!(file.theme, None);
    assert_eq!(
        file.warnings,
        vec!["ignored `theme`: must not be empty".to_string()]
    );
}

#[test]
fn a_whitespace_only_theme_name_is_skipped_with_a_warning() {
    // `theme "   "` is treated as blank, not as a name of three spaces.
    let file = parse_file("theme \"   \"");
    assert_eq!(file.theme, None);
    assert_eq!(
        file.warnings,
        vec!["ignored `theme`: must not be empty".to_string()]
    );
}

#[test]
fn a_non_string_theme_name_is_skipped_with_a_warning() {
    let file = parse_file("theme 42");
    assert_eq!(file.theme, None);
    assert_eq!(
        file.warnings,
        vec!["ignored `theme`: expected a string".to_string()]
    );
}

#[test]
fn a_repeated_theme_line_keeps_the_first_and_warns() {
    // `theme` may appear once like the rest: the later line is dropped and
    // the first stands.
    let file = parse_file("theme \"midnight\"\ntheme \"solarized\"");
    assert_eq!(file.theme, Some("midnight".to_string()));
    assert_eq!(
        file.warnings,
        vec!["ignored duplicate `theme` section".to_string()]
    );
}

#[test]
fn allow_beta_features_records_what_it_is_set_to() {
    assert_eq!(
        parse("allow-beta-features #true").allow_beta_features,
        Some(true)
    );
    assert_eq!(
        parse("allow-beta-features #false").allow_beta_features,
        Some(false)
    );
}

#[test]
fn an_absent_allow_beta_features_sets_no_layer() {
    // Absent leaves the field unset; the built-in `false` stands.
    assert_eq!(parse("").allow_beta_features, None);
}

#[test]
fn a_non_boolean_allow_beta_features_is_skipped_with_a_warning() {
    let (layer, warnings) = parse_with_warnings("allow-beta-features \"yes\"");
    assert_eq!(layer.allow_beta_features, None);
    assert_eq!(
        warnings,
        vec!["ignored `allow-beta-features`: expected a boolean (#true or #false)".to_string()]
    );
}

#[test]
fn a_repeated_allow_beta_features_line_keeps_the_first_and_warns() {
    let (layer, warnings) =
        parse_with_warnings("allow-beta-features #true\nallow-beta-features #false");
    assert_eq!(layer.allow_beta_features, Some(true));
    assert_eq!(
        warnings,
        vec!["ignored duplicate `allow-beta-features` section".to_string()]
    );
}

#[test]
fn allow_other_users_records_what_it_is_set_to() {
    assert_eq!(
        parse("allow-other-users #true").allow_other_users,
        Some(true)
    );
    assert_eq!(
        parse("allow-other-users #false").allow_other_users,
        Some(false)
    );
}

#[test]
fn an_absent_allow_other_users_sets_no_layer() {
    // Absent leaves the field unset; the built-in `false` stands.
    assert_eq!(parse("").allow_other_users, None);
}

#[test]
fn a_non_boolean_allow_other_users_is_skipped_with_a_warning() {
    let (layer, warnings) = parse_with_warnings("allow-other-users \"yes\"");
    assert_eq!(layer.allow_other_users, None);
    assert_eq!(
        warnings,
        vec!["ignored `allow-other-users`: expected a boolean (#true or #false)".to_string()]
    );
}

#[test]
fn a_repeated_allow_other_users_line_keeps_the_first_and_warns() {
    let (layer, warnings) =
        parse_with_warnings("allow-other-users #true\nallow-other-users #false");
    assert_eq!(layer.allow_other_users, Some(true));
    assert_eq!(
        warnings,
        vec!["ignored duplicate `allow-other-users` section".to_string()]
    );
}

#[test]
fn remote_listen_records_the_address_it_names() {
    assert_eq!(
        parse("remote-listen \"127.0.0.1:7654\"").remote_listen,
        Some(Some("127.0.0.1:7654".to_string()))
    );
}

#[test]
fn an_absent_remote_listen_sets_no_layer() {
    // Absent leaves the field unset; no address is named.
    assert_eq!(parse("").remote_listen, None);
}

#[test]
fn a_non_string_remote_listen_is_skipped_with_a_warning() {
    let (layer, warnings) = parse_with_warnings("remote-listen 7654");
    assert_eq!(layer.remote_listen, None);
    assert_eq!(
        warnings,
        vec!["ignored `remote-listen`: expected a string".to_string()]
    );
}

#[test]
fn a_blank_remote_listen_is_skipped_with_a_warning() {
    // An empty value is skipped; the field stays unset.
    let (layer, warnings) = parse_with_warnings("remote-listen \"\"");
    assert_eq!(layer.remote_listen, None);
    assert_eq!(
        warnings,
        vec!["ignored `remote-listen`: must not be empty".to_string()]
    );
}

#[test]
fn a_repeated_remote_listen_line_keeps_the_first_and_warns() {
    let (layer, warnings) =
        parse_with_warnings("remote-listen \"127.0.0.1:7654\"\nremote-listen \"0.0.0.0:9000\"");
    assert_eq!(
        layer.remote_listen,
        Some(Some("127.0.0.1:7654".to_string()))
    );
    assert_eq!(
        warnings,
        vec!["ignored duplicate `remote-listen` section".to_string()]
    );
}

#[test]
fn shared_sessions_dir_records_the_directory_it_names() {
    assert_eq!(
        parse("shared-sessions-dir \"/var/run/koshi\"").shared_sessions_dir,
        Some(Some(PathBuf::from("/var/run/koshi")))
    );
}

#[test]
fn an_absent_shared_sessions_dir_sets_no_layer() {
    // Absent leaves the field unset; the platform's machine-wide directory
    // stands.
    assert_eq!(parse("").shared_sessions_dir, None);
}

#[test]
fn a_non_string_shared_sessions_dir_is_skipped_with_a_warning() {
    let (layer, warnings) = parse_with_warnings("shared-sessions-dir 42");
    assert_eq!(layer.shared_sessions_dir, None);
    assert_eq!(
        warnings,
        vec!["ignored `shared-sessions-dir`: expected a string".to_string()]
    );
}

#[test]
fn a_blank_shared_sessions_dir_is_skipped_with_a_warning() {
    // An empty value is skipped; the platform's machine-wide directory stands.
    let (layer, warnings) = parse_with_warnings("shared-sessions-dir \"\"");
    assert_eq!(layer.shared_sessions_dir, None);
    assert_eq!(
        warnings,
        vec!["ignored `shared-sessions-dir`: must not be empty".to_string()]
    );
}

#[test]
fn a_repeated_shared_sessions_dir_line_keeps_the_first_and_warns() {
    let (layer, warnings) = parse_with_warnings(
        "shared-sessions-dir \"/var/run/koshi\"\nshared-sessions-dir \"/tmp/koshi\"",
    );
    assert_eq!(
        layer.shared_sessions_dir,
        Some(Some(PathBuf::from("/var/run/koshi")))
    );
    assert_eq!(
        warnings,
        vec!["ignored duplicate `shared-sessions-dir` section".to_string()]
    );
}

#[test]
fn auto_close_session_records_what_it_is_set_to() {
    assert_eq!(
        parse("auto-close-session #true").auto_close_session,
        Some(true)
    );
    assert_eq!(
        parse("auto-close-session #false").auto_close_session,
        Some(false)
    );
}

#[test]
fn an_absent_auto_close_session_sets_no_layer() {
    // Absent leaves the field unset; the built-in `false` stands.
    assert_eq!(parse("").auto_close_session, None);
}

#[test]
fn a_non_boolean_auto_close_session_is_skipped_with_a_warning() {
    let (layer, warnings) = parse_with_warnings("auto-close-session \"yes\"");
    assert_eq!(layer.auto_close_session, None);
    assert_eq!(
        warnings,
        vec!["ignored `auto-close-session`: expected a boolean (#true or #false)".to_string()]
    );
}

#[test]
fn a_colors_block_in_the_app_file_is_ignored() {
    // Colors belong to a theme file. An inline `colors` block in `koshi.kdl`
    // is an unknown top-level node and sets nothing.
    let file = parse_file("theme \"midnight\"\ncolors {\n    accent \"#ff0000\"\n}");
    assert_eq!(file.theme, Some("midnight".to_string()));
    assert_eq!(file.layer.theme, None);
    assert_eq!(
        file.warnings,
        vec!["ignored unknown key `colors`; did you mean `copy`?".to_string()]
    );
}

#[test]
fn reads_all_update_fields() {
    let update =
        parse("update {\n    auto-check #false\n    check-interval-days 30\n    allow-prerelease #true\n}")
            .update
            .expect("update section present");
    assert_eq!(update.auto_check, Some(false));
    assert_eq!(update.check_interval_days, Some(30));
    assert_eq!(update.allow_prerelease, Some(true));
}

#[test]
fn a_non_boolean_allow_prerelease_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    allow-prerelease 1\n}",
    )
    .expect_err("integer is not a boolean");
    assert_validation(
        error,
        "allow-prerelease",
        "expected a boolean (#true or #false)",
    );
}

#[test]
fn update_block_sets_only_the_fields_it_lists() {
    let update = parse("update {\n    auto-check #true\n}")
        .update
        .expect("update section present");
    assert_eq!(update.auto_check, Some(true));
    assert_eq!(update.check_interval_days, None);
}

#[test]
fn empty_update_block_sets_an_all_none_section() {
    let update = parse("update {\n}").update.expect("update section present");
    assert_eq!(update.auto_check, None);
    assert_eq!(update.check_interval_days, None);
}

#[test]
fn an_unknown_top_level_node_warns_with_a_suggestion() {
    let (layer, warnings) = parse_with_warnings("frobnicate {\n    whatever 5\n}");
    assert_eq!(layer.update, None);
    assert_eq!(layer.mouse, None);
    assert_eq!(
        warnings,
        vec!["ignored unknown key `frobnicate`; did you mean `update`?".to_string()]
    );
}

#[test]
fn unknown_field_inside_update_warns() {
    let (layer, warnings) =
        parse_with_warnings("update {\n    auto-check #true\n    frequency 5\n}");
    let update = layer.update.expect("update section present");
    assert_eq!(update.auto_check, Some(true));
    assert_eq!(update.check_interval_days, None);
    assert_eq!(
        warnings,
        ["ignored unknown key `update.frequency`; did you mean `update.auto-check`?"]
    );
}

#[test]
fn non_boolean_auto_check_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    auto-check \"yes\"\n}",
    )
    .expect_err("string is not a boolean");
    assert_validation(error, "auto-check", "expected a boolean (#true or #false)");
}

#[test]
fn non_integer_interval_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    check-interval-days #true\n}",
    )
    .expect_err("boolean is not an integer");
    assert_validation(error, "check-interval-days", "expected an integer");
}

#[test]
fn negative_interval_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    check-interval-days -3\n}",
    )
    .expect_err("negative does not fit u32");
    assert_validation(
        error,
        "check-interval-days",
        "must be between 0 and 4294967295",
    );
}

#[test]
fn extra_argument_on_a_field_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    check-interval-days 3 9\n}",
    )
    .expect_err("two values is not one");
    assert_validation(error, "check-interval-days", "expected exactly one value");
}

#[test]
fn a_duplicate_update_section_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    auto-check #true\n}\nupdate {\n    auto-check #false\n}",
    )
    .expect_err("two update sections");
    assert_validation(error, "update", "`update` is declared more than once");
}

#[test]
fn a_current_schema_version_is_accepted() {
    let layer = parse("version 1\nupdate {\n    auto-check #false\n}");
    assert_eq!(
        layer.update.expect("update section present").auto_check,
        Some(false)
    );
}

#[test]
fn a_newer_schema_version_is_a_validation_error() {
    let error = parse_app_config(Path::new("koshi.kdl"), "version 999")
        .expect_err("version newer than this build");
    assert_validation(
        error,
        "version",
        "config schema version 999 is newer than this koshi supports (1)",
    );
}

#[test]
fn a_duplicate_version_is_a_hard_error() {
    // A second `version` is an error, not a skippable duplicate section: the
    // parse fails before the sections after it apply.
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "version 1\nversion 999\npane {\n    min-cols 5\n}",
    )
    .expect_err("duplicate version rejected");
    assert_validation(error, "version", "`version` is declared more than once");
}

#[test]
fn a_version_with_children_is_a_validation_error() {
    let error = parse_app_config(Path::new("koshi.kdl"), "version 1 {}")
        .expect_err("version children rejected");
    assert_validation(error, "version", "`version` takes no children");
}

#[test]
fn syntax_error_is_a_parse_error() {
    let source = "update { auto-check #true";
    let error = parse_app_config(Path::new("koshi.kdl"), source).expect_err("unclosed block");

    let ConfigError::Parse { path, detail } = error else {
        panic!("expected a parse error, got {error:?}");
    };
    assert_eq!(path, "koshi.kdl");
    assert_eq!(detail, kdl_first_diagnostic(source));
}

// --- Field-partial sections ---------------------------------------------------

#[test]
fn scrollback_section_parses_both_fields() {
    let scrollback = parse("scrollback {\n    max-lines 50000\n    max-bytes 1048576\n}")
        .scrollback
        .expect("scrollback section present");
    assert_eq!(scrollback.max_lines, Some(50000));
    assert_eq!(scrollback.max_bytes, Some(1_048_576));
}

#[test]
fn scrollback_scroll_on_input_parses() {
    let scrollback = parse("scrollback {\n    scroll-on-input #false\n}")
        .scrollback
        .expect("scrollback section present");
    assert_eq!(scrollback.scroll_on_input, Some(false));
}

#[test]
fn layout_new_pane_direction_parses() {
    let layout = parse("layout {\n    new-pane-direction \"down\"\n}")
        .layout
        .expect("layout section present");
    assert_eq!(layout.new_pane_direction, Some(Direction::Down));
}

#[test]
fn mouse_section_parses_every_field() {
    let mouse =
        parse("mouse {\n    border-resize #false\n    scroll-lines 5\n    wheel \"ignore\"\n}")
            .mouse
            .expect("mouse section present");
    assert_eq!(mouse.border_resize, Some(false));
    assert_eq!(mouse.scroll_lines, Some(5));
    assert_eq!(mouse.wheel, Some(WheelScroll::Ignore));
}

#[test]
fn remote_reconnect_records_what_it_is_set_to() {
    assert_eq!(parse("remote-reconnect #true").remote_reconnect, Some(true));
    assert_eq!(
        parse("remote-reconnect #false").remote_reconnect,
        Some(false)
    );
}

#[test]
fn an_absent_remote_reconnect_leaves_dialing_again_on() {
    // Absent leaves the field unset; the built-in `true` stands.
    assert_eq!(parse("").remote_reconnect, None);
    assert!(ClientConfig::default().remote_reconnect);
}

#[test]
fn a_non_boolean_remote_reconnect_is_skipped_with_a_warning() {
    let (layer, warnings) = parse_with_warnings("remote-reconnect \"yes\"");
    assert_eq!(layer.remote_reconnect, None);
    assert_eq!(
        warnings,
        vec!["ignored `remote-reconnect`: expected a boolean (#true or #false)".to_string()]
    );
}

#[test]
fn a_repeated_remote_reconnect_line_keeps_the_first_and_warns() {
    let (layer, warnings) = parse_with_warnings("remote-reconnect #false\nremote-reconnect #true");
    assert_eq!(layer.remote_reconnect, Some(false));
    assert_eq!(
        warnings,
        vec!["ignored duplicate `remote-reconnect` section".to_string()]
    );
}

#[test]
fn copy_section_parses() {
    let copy = parse("copy {\n    trim-trailing-whitespace #false\n}")
        .copy
        .expect("copy section present");
    assert_eq!(copy.trim_trailing_whitespace, Some(false));
}

#[test]
fn terminal_section_parses_including_default_shell() {
    let terminal =
        parse("terminal {\n    term \"xterm\"\n    colorterm \"truecolor\"\n    default-shell \"/bin/zsh\"\n}")
            .terminal
            .expect("terminal section present");
    assert_eq!(terminal.term, Some("xterm".to_string()));
    assert_eq!(terminal.colorterm, Some("truecolor".to_string()));
    assert_eq!(terminal.default_shell, Some(Some("/bin/zsh".to_string())));
}

#[test]
fn a_blank_term_or_colorterm_is_skipped_so_the_built_in_identity_stands() {
    // A blank or whitespace-only `term`/`colorterm` is dropped like any bad
    // field; the built-in `xterm-256color`/`truecolor` identity applies.
    let (layer, warnings) =
        parse_with_warnings("terminal {\n    term \"\"\n    colorterm \"  \"\n}");
    let terminal = layer.terminal.expect("terminal section present");
    assert_eq!(terminal.term, None);
    assert_eq!(terminal.colorterm, None);
    assert_eq!(
        warnings,
        vec![
            "ignored `terminal.term`: must not be empty".to_string(),
            "ignored `terminal.colorterm`: must not be empty".to_string(),
        ]
    );
}

#[test]
fn an_empty_default_shell_is_skipped_so_the_shell_falls_back_to_the_environment() {
    // An empty or whitespace-only `default-shell` is dropped like any bad
    // field; `default_shell` stays unset and the spawn path falls back to
    // `$SHELL`/`%COMSPEC%`.
    let (layer, warnings) = parse_with_warnings("terminal {\n    default-shell \"\"\n}");
    assert_eq!(
        layer.terminal.and_then(|terminal| terminal.default_shell),
        None
    );
    assert_eq!(
        warnings,
        vec!["ignored `terminal.default-shell`: must not be empty".to_string()]
    );
}

#[test]
fn surrounding_whitespace_is_trimmed_off_every_nonempty_string_field() {
    // The value is stored trimmed of surrounding whitespace, the field still
    // applies, and no warning is raised.
    let file = parse_file(
        "theme \"  midnight  \"\n\
         terminal {\n\
         term \" xterm-256color \"\n\
         colorterm \"\\ttruecolor \"\n\
         default-shell \" /bin/zsh \"\n\
         }",
    );
    assert_eq!(file.theme, Some("midnight".to_string()));
    let terminal = file.layer.terminal.expect("terminal section present");
    assert_eq!(terminal.term, Some("xterm-256color".to_string()));
    assert_eq!(terminal.colorterm, Some("truecolor".to_string()));
    assert_eq!(terminal.default_shell, Some(Some("/bin/zsh".to_string())));
    assert!(
        file.warnings.is_empty(),
        "trimming is not a skip: {:?}",
        file.warnings
    );
}

#[test]
fn inner_whitespace_in_a_string_field_is_left_alone() {
    // Only the ends are trimmed; whitespace inside the value survives intact.
    let terminal = parse("terminal {\n    default-shell \"/Applications/My Shell/bin/sh\"\n}")
        .terminal
        .expect("terminal section present");
    assert_eq!(
        terminal.default_shell,
        Some(Some("/Applications/My Shell/bin/sh".to_string()))
    );
}

#[test]
fn logging_section_parses() {
    let logging =
        parse("logging {\n    enabled #true\n    level \"error\"\n    format \"json\"\n}")
            .logging
            .expect("logging section present");
    assert_eq!(logging.enabled, Some(true));
    assert_eq!(logging.level, Some(LogLevel::Error));
    assert_eq!(logging.format, Some(LogFormat::Json));
}

#[test]
fn logging_level_and_format_accept_each_variant() {
    for (text, level) in [
        ("info", LogLevel::Info),
        ("warning", LogLevel::Warning),
        ("error", LogLevel::Error),
    ] {
        let logging = parse(&format!("logging {{\n    level \"{text}\"\n}}"))
            .logging
            .expect("logging section present");
        assert_eq!(logging.level, Some(level), "level {text}");
    }
    for (text, format) in [("pretty", LogFormat::Pretty), ("json", LogFormat::Json)] {
        let logging = parse(&format!("logging {{\n    format \"{text}\"\n}}"))
            .logging
            .expect("logging section present");
        assert_eq!(logging.format, Some(format), "format {text}");
    }
}

#[test]
fn a_bad_logging_level_is_skipped_with_a_warning() {
    // `verbose` is not a level: it is dropped with a warning, and `enabled`
    // beside it still applies.
    let (layer, warnings) =
        parse_with_warnings("logging {\n    level \"verbose\"\n    enabled #true\n}");
    let logging = layer.logging.expect("logging section present");
    assert_eq!(logging.level, None, "the bad level is dropped");
    assert_eq!(
        logging.enabled,
        Some(true),
        "the sibling field still applies"
    );
    assert_eq!(
        warnings,
        vec![r#"ignored `logging.level`: expected "info", "warning", or "error""#.to_string()]
    );
}

#[test]
fn a_bad_field_is_skipped_and_the_rest_of_the_section_applies() {
    // `max-lines` is a string where a number is required: it is dropped, but
    // `max-bytes` beside it still applies, and the whole parse still succeeds.
    let (layer, warnings) =
        parse_with_warnings("scrollback {\n    max-lines \"oops\"\n    max-bytes 500\n}");
    let scrollback = layer.scrollback.expect("scrollback section present");
    assert_eq!(scrollback.max_lines, None);
    assert_eq!(scrollback.max_bytes, Some(500));
    assert_eq!(
        warnings,
        vec!["ignored `scrollback.max-lines`: expected an integer".to_string()]
    );
}

#[test]
fn a_negative_scrollback_cap_becomes_zero() {
    // A negative cap is clamped to 0 ("no scrollback"), with no warning.
    let (layer, warnings) = parse_with_warnings("scrollback {\n    max-lines -5\n}");
    assert_eq!(
        layer.scrollback.expect("scrollback present").max_lines,
        Some(0)
    );
    assert!(warnings.is_empty(), "a negative cap is clamped, not warned");
}

#[test]
fn a_bad_direction_value_is_skipped_with_a_warning() {
    let (layer, warnings) = parse_with_warnings("layout {\n    new-pane-direction \"sideways\"\n}");
    assert_eq!(
        layer
            .layout
            .expect("layout section present")
            .new_pane_direction,
        None
    );
    assert_eq!(
        warnings,
        vec![
            r#"ignored `layout.new-pane-direction`: expected "left", "right", "up", or "down""#
                .to_string()
        ]
    );
}

#[test]
fn an_unknown_field_in_a_section_warns() {
    let (_, warnings) = parse_with_warnings("scrollback {\n    frequency 5\n}");
    assert_eq!(
        warnings,
        vec![
            "ignored unknown key `scrollback.frequency`; did you mean `scrollback.max-lines`?"
                .to_string()
        ]
    );
}

#[test]
fn a_duplicate_field_partial_section_warns_and_keeps_the_first() {
    let (layer, warnings) = parse_with_warnings(
        "scrollback {\n    max-lines 100\n}\nscrollback {\n    max-lines 200\n}",
    );
    assert_eq!(
        layer.scrollback.expect("scrollback present").max_lines,
        Some(100)
    );
    assert_eq!(
        warnings,
        vec!["ignored duplicate `scrollback` section".to_string()]
    );
}

#[test]
fn the_pane_gap_line_records_its_value() {
    assert_eq!(
        parse("pane {\n    gap 3\n}")
            .pane
            .expect("pane present")
            .gap,
        Some(3)
    );
}

#[test]
fn a_pane_block_without_a_gap_line_leaves_the_gap_unset() {
    assert_eq!(
        parse("pane {\n    min-cols 2\n}")
            .pane
            .expect("pane present")
            .gap,
        None
    );
}

// -- adversarial: type confusion and bounds -------------------------------

#[test]
fn a_string_pane_dimension_is_skipped_as_a_non_integer() {
    let (layer, warnings) = parse_with_warnings("pane {\n    min-cols \"wide\"\n}");
    assert_eq!(layer.pane.expect("pane present").min_cols, None);
    assert_eq!(
        warnings,
        vec!["ignored `pane.min-cols`: expected an integer".to_string()]
    );
}

#[test]
fn a_string_pane_gap_is_skipped_as_a_non_integer() {
    let (layer, warnings) = parse_with_warnings("pane {\n    gap \"wide\"\n}");
    assert_eq!(layer.pane.expect("pane present").gap, None);
    assert_eq!(
        warnings,
        vec!["ignored `pane.gap`: expected an integer".to_string()]
    );
}

#[test]
fn a_negative_pane_gap_is_skipped_not_clamped() {
    let (layer, warnings) = parse_with_warnings("pane {\n    gap -1\n}");
    assert_eq!(layer.pane.expect("pane present").gap, None);
    assert_eq!(
        warnings,
        vec!["ignored `pane.gap`: must be between 0 and 65535".to_string()]
    );
}

#[test]
fn a_pane_gap_past_the_cell_count_limit_is_skipped() {
    let (layer, warnings) = parse_with_warnings("pane {\n    gap 70000\n}");
    assert_eq!(layer.pane.expect("pane present").gap, None);
    assert_eq!(
        warnings,
        vec!["ignored `pane.gap`: must be between 0 and 65535".to_string()]
    );
}

#[test]
fn an_out_of_range_pane_dimension_is_skipped_with_the_range_reason() {
    // 70000 overflows u16 and -1 underflows it; both are skipped with the same
    // bound reason, and the default dimension stands.
    let (layer, warnings) = parse_with_warnings("pane {\n    min-cols 70000\n    min-rows -1\n}");
    let pane = layer.pane.expect("pane present");
    assert_eq!(pane.min_cols, None);
    assert_eq!(pane.min_rows, None);
    assert_eq!(
        warnings,
        vec![
            "ignored `pane.min-cols`: must be between 0 and 65535".to_string(),
            "ignored `pane.min-rows`: must be between 0 and 65535".to_string(),
        ]
    );
}

#[test]
fn a_negative_scroll_lines_is_skipped_not_clamped() {
    // Scroll lines is a plain u16 field: a negative value is dropped with a
    // warning. Only the scrollback caps clamp.
    let (layer, warnings) = parse_with_warnings("mouse {\n    scroll-lines -5\n}");
    assert_eq!(layer.mouse.expect("mouse present").scroll_lines, None);
    assert_eq!(
        warnings,
        vec!["ignored `mouse.scroll-lines`: must be between 0 and 65535".to_string()]
    );
}

#[test]
fn a_garbage_version_value_is_a_validation_error() {
    let error = parse_app_config(Path::new("koshi.kdl"), "version \"abc\"")
        .expect_err("string is not a version integer");
    assert_validation(
        error,
        "version",
        "`version` must be an integer from 1 to 4294967295",
    );
}

#[test]
fn a_negative_version_is_a_validation_error() {
    let error = parse_app_config(Path::new("koshi.kdl"), "version -1")
        .expect_err("a negative version does not fit u32");
    assert_validation(
        error,
        "version",
        "`version` must be an integer from 1 to 4294967295",
    );
}

#[test]
fn a_comments_only_file_sets_no_layer_and_no_warnings() {
    let (layer, warnings) = parse_with_warnings("// only a comment\n// and another\n");
    assert_eq!(layer, PartialKoshiConfig::default());
    assert!(warnings.is_empty());
}

#[test]
fn carriage_return_line_endings_parse_like_line_feeds() {
    // A file written on Windows arrives with CRLF; it must parse identically to
    // the same content with plain LF, on every platform.
    let lf = "scrollback {\n    max-lines 123\n}\n";
    let crlf = "scrollback {\r\n    max-lines 123\r\n}\r\n";
    let from_lf = parse(lf).scrollback.expect("lf scrollback present");
    let from_crlf = parse(crlf).scrollback.expect("crlf scrollback present");
    assert_eq!(from_lf.max_lines, Some(123));
    assert_eq!(from_crlf.max_lines, Some(123));
}

#[test]
fn a_whitespace_only_file_sets_no_layer_and_no_warnings() {
    // Spaces, tabs, and blank lines with no node at all leave every section
    // unset, the same as an empty file, and raise no warning.
    let (layer, warnings) = parse_with_warnings("   \n\t  \n \n");
    assert_eq!(layer, PartialKoshiConfig::default());
    assert!(warnings.is_empty());
}

#[test]
fn a_leading_byte_order_mark_is_tolerated() {
    // An editor that saves UTF-8 with a leading BOM prepends U+FEFF; the file
    // must still parse, with the first section read exactly as if the BOM were
    // absent, on every platform.
    let (layer, warnings) = parse_with_warnings("\u{feff}scrollback {\n    max-lines 5\n}");
    assert_eq!(
        layer.scrollback.expect("scrollback present").max_lines,
        Some(5)
    );
    assert!(warnings.is_empty());
}

#[test]
fn a_float_where_an_integer_is_required_is_skipped_as_a_non_integer() {
    // `1.5` parses as a KDL float, which is the wrong kind for a u16 dimension:
    // the field is dropped with the same reason a string would give, and the
    // default dimension stands.
    let (layer, warnings) = parse_with_warnings("pane {\n    min-cols 1.5\n}");
    assert_eq!(layer.pane.expect("pane present").min_cols, None);
    assert_eq!(
        warnings,
        vec!["ignored `pane.min-cols`: expected an integer".to_string()]
    );
}

#[test]
fn the_largest_u32_interval_is_accepted_and_one_past_it_is_a_validation_error() {
    // u32's ceiling, 4294967295, fits the strict `update` interval field; one
    // more overflows it and fails the whole parse with the range reason.
    let at_max = parse("update {\n    check-interval-days 4294967295\n}")
        .update
        .expect("update present");
    assert_eq!(at_max.check_interval_days, Some(4_294_967_295));

    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    check-interval-days 4294967296\n}",
    )
    .expect_err("one past u32 max");
    assert_validation(
        error,
        "check-interval-days",
        "must be between 0 and 4294967295",
    );
}

#[test]
fn the_first_unsupported_schema_version_is_rejected_at_the_boundary() {
    // The build supports schema version 1; version 2, one past the boundary,
    // is the smallest rejected version, named in the exact detail.
    let error = parse_app_config(Path::new("koshi.kdl"), "version 2")
        .expect_err("version 2 is newer than this build");
    assert_validation(
        error,
        "version",
        "config schema version 2 is newer than this koshi supports (1)",
    );
}

#[test]
fn a_repeated_field_inside_one_section_keeps_the_last_value() {
    // Two `min-cols` lines in one `pane` block is not a duplicate section: the
    // later value wins and no warning is raised, matching KDL's last-node-wins
    // reading of repeated fields.
    let (layer, warnings) = parse_with_warnings("pane {\n    min-cols 5\n    min-cols 10\n}");
    assert_eq!(layer.pane.expect("pane present").min_cols, Some(10));
    assert!(warnings.is_empty());
}

#[test]
fn a_field_with_no_value_is_skipped_with_a_warning() {
    // A bare `min-cols` with no argument has zero values where exactly one is
    // required: it is dropped like any bad field and the default stands.
    let (layer, warnings) = parse_with_warnings("pane {\n    min-cols\n}");
    assert_eq!(layer.pane.expect("pane present").min_cols, None);
    assert_eq!(
        warnings,
        vec!["ignored `pane.min-cols`: expected exactly one value".to_string()]
    );
}

#[test]
fn an_unterminated_quote_is_a_parse_error_not_a_panic() {
    // A string value left open by a newline is a KDL lexer error, surfaced as
    // a parse error.
    let source = "terminal {\n    term \"xterm\n}";
    let error = parse_app_config(Path::new("koshi.kdl"), source).expect_err("unterminated string");

    let ConfigError::Parse { path, detail } = error else {
        panic!("expected a parse error, got {error:?}");
    };
    assert_eq!(path, "koshi.kdl");
    assert_eq!(detail, kdl_first_diagnostic(source));
}

#[test]
fn hostile_byte_sequences_never_panic_and_a_later_valid_parse_still_succeeds() {
    // The parser is a trust boundary: user-authored bytes must always return a
    // result, never panic. Each of these malformed inputs is parsed only for
    // its no-panic effect (a panic would fail the test); the value is ignored.
    // Deeply nested blocks are excluded: the KDL parser recurses per nesting
    // level and 100 nested `{` overflow a test thread's stack.
    let hostile: &[&str] = &[
        "\0",
        "{",
        "}",
        "pane {",
        "\"unterminated",
        "pane {\n    min-cols=5\n}",
        "pane {\n    min-cols \0\n}",
        "\u{feff}\u{feff}\u{feff}",
        "scrollback {\n    max-lines 999999999999999999999999999999\n}",
        &format!(
            "// {}\nscrollback {{\n    max-lines 7\n}}",
            "x".repeat(200_000)
        ),
        &"\u{4f60}\u{597d}".repeat(500),
        "version\tupdate\tpane",
    ];
    for src in hostile {
        // Discarded on purpose: the only property under test is that the call
        // returns instead of panicking or aborting.
        let _ = parse_app_config(Path::new("koshi.kdl"), src);
    }

    // After the barrage, an ordinary config still parses to the right value —
    // the parser holds no poisoned state between calls.
    let good = parse("scrollback {\n    max-lines 4242\n}");
    assert_eq!(
        good.scrollback.expect("scrollback present").max_lines,
        Some(4242)
    );
}

#[test]
fn version_zero_is_a_validation_error() {
    let error = parse_app_config(Path::new("koshi.kdl"), "version 0")
        .expect_err("schema versions start at one");
    assert_validation(error, "version", "config schema version must be at least 1");
}

#[test]
fn a_version_with_a_named_property_is_a_validation_error() {
    // `version v=1` carries a named property where one plain value is required.
    let error = parse_app_config(Path::new("koshi.kdl"), "version v=1")
        .expect_err("a property is not a plain value");
    assert_validation(
        error,
        "version",
        "`version` takes exactly one integer argument",
    );
}

#[test]
fn a_version_after_the_last_section_still_counts() {
    // `version` may appear anywhere in the file, not only on the first line.
    let layer = parse("pane {\n    min-cols 2\n}\nversion 1");
    assert_eq!(layer.pane.expect("pane present").min_cols, Some(2));
}

#[test]
fn a_section_line_with_no_block_sets_an_all_none_section() {
    // A bare `pane` line has no `{ … }` block: the section is marked present
    // with every field unset, and no warning is raised.
    let (layer, warnings) = parse_with_warnings("pane");
    assert_eq!(layer.pane, Some(PartialPaneConfig::default()));
    assert!(warnings.is_empty());
}

#[test]
fn a_bare_update_line_sets_an_all_none_section() {
    let (layer, warnings) = parse_with_warnings("update");
    assert_eq!(layer.update, Some(PartialUpdateConfig::default()));
    assert!(warnings.is_empty());
}

#[test]
fn a_theme_line_with_a_child_block_is_skipped_with_a_warning() {
    let file = parse_file("theme \"midnight\" {\n}");
    assert_eq!(file.theme, None);
    assert_eq!(
        file.warnings,
        vec!["ignored `theme`: takes no children".to_string()]
    );
}

#[test]
fn a_theme_line_with_no_value_is_skipped_with_a_warning() {
    let file = parse_file("theme");
    assert_eq!(file.theme, None);
    assert_eq!(
        file.warnings,
        vec!["ignored `theme`: expected exactly one value".to_string()]
    );
}

#[test]
fn remote_listen_and_shared_sessions_dir_values_are_trimmed() {
    let file =
        parse_file("remote-listen \" 127.0.0.1:7654 \"\nshared-sessions-dir \" /var/run/koshi \"");
    assert_eq!(
        file.layer.remote_listen,
        Some(Some("127.0.0.1:7654".to_string()))
    );
    assert_eq!(
        file.layer.shared_sessions_dir,
        Some(Some(PathBuf::from("/var/run/koshi")))
    );
    assert!(file.warnings.is_empty());
}

#[test]
fn an_update_field_with_a_child_block_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    auto-check #true {\n    }\n}",
    )
    .expect_err("a field carries no block");
    assert_validation(error, "auto-check", "takes no children");
}

#[test]
fn a_repeated_update_field_keeps_the_last_value() {
    // Two `auto-check` lines in one `update` block read like repeated fields
    // in a field-partial section: the later value wins, with no warning.
    let (layer, warnings) =
        parse_with_warnings("update {\n    auto-check #true\n    auto-check #false\n}");
    assert_eq!(
        layer.update.expect("update present").auto_check,
        Some(false)
    );
    assert!(warnings.is_empty());
}

#[test]
fn unknown_top_level_nodes_warn_once_each() {
    // Unknown names are not deduplicated: each occurrence warns.
    let (_, warnings) = parse_with_warnings("frobnicate 1\nfrobnicate 2");
    assert_eq!(
        warnings,
        vec![
            "ignored unknown key `frobnicate`; did you mean `update`?".to_string(),
            "ignored unknown key `frobnicate`; did you mean `update`?".to_string(),
        ]
    );
}

#[test]
fn warnings_arrive_in_file_order() {
    let (_, warnings) =
        parse_with_warnings("theme \"\"\npane {\n    min-cols \"x\"\n}\nfrobnicate 1");
    assert_eq!(
        warnings,
        vec![
            "ignored `theme`: must not be empty".to_string(),
            "ignored `pane.min-cols`: expected an integer".to_string(),
            "ignored unknown key `frobnicate`; did you mean `update`?".to_string(),
        ]
    );
}

#[test]
fn a_scrollback_cap_above_usize_max_becomes_usize_max() {
    // 18446744073709551616 is one past u64::MAX; the cap saturates at
    // usize::MAX with no warning.
    let (layer, warnings) =
        parse_with_warnings("scrollback {\n    max-lines 18446744073709551616\n}");
    assert_eq!(
        layer.scrollback.expect("scrollback present").max_lines,
        Some(usize::MAX)
    );
    assert!(warnings.is_empty());
}

#[test]
fn scroll_lines_at_the_u16_ceiling_is_accepted_and_one_past_is_skipped() {
    let at_max = parse("mouse {\n    scroll-lines 65535\n}");
    assert_eq!(
        at_max.mouse.expect("mouse present").scroll_lines,
        Some(65535)
    );

    let (layer, warnings) = parse_with_warnings("mouse {\n    scroll-lines 65536\n}");
    assert_eq!(layer.mouse.expect("mouse present").scroll_lines, None);
    assert_eq!(
        warnings,
        vec!["ignored `mouse.scroll-lines`: must be between 0 and 65535".to_string()]
    );
}

#[test]
fn a_field_with_a_named_property_is_skipped_with_a_warning() {
    // `min-cols x=5` carries a named property where one plain value is
    // required.
    let (layer, warnings) = parse_with_warnings("pane {\n    min-cols x=5\n}");
    assert_eq!(layer.pane.expect("pane present").min_cols, None);
    assert_eq!(
        warnings,
        vec!["ignored `pane.min-cols`: expected exactly one value".to_string()]
    );
}

#[test]
fn wheel_accepts_each_variant() {
    for (text, wheel) in [
        ("scroll-scrollback", WheelScroll::ScrollScrollback),
        ("ignore", WheelScroll::Ignore),
    ] {
        let mouse = parse(&format!("mouse {{\n    wheel \"{text}\"\n}}"))
            .mouse
            .expect("mouse section present");
        assert_eq!(mouse.wheel, Some(wheel), "wheel {text}");
    }
}

#[test]
fn a_bad_wheel_value_is_skipped_with_a_warning() {
    let (layer, warnings) = parse_with_warnings("mouse {\n    wheel \"zoom\"\n}");
    assert_eq!(layer.mouse.expect("mouse present").wheel, None);
    assert_eq!(
        warnings,
        vec![r#"ignored `mouse.wheel`: expected "scroll-scrollback" or "ignore""#.to_string()]
    );
}

#[test]
fn a_bad_logging_format_is_skipped_with_a_warning() {
    let (layer, warnings) = parse_with_warnings("logging {\n    format \"xml\"\n}");
    assert_eq!(layer.logging.expect("logging present").format, None);
    assert_eq!(
        warnings,
        vec![r#"ignored `logging.format`: expected "pretty" or "json""#.to_string()]
    );
}

#[test]
fn new_pane_direction_accepts_each_variant() {
    for (text, direction) in [
        ("left", Direction::Left),
        ("right", Direction::Right),
        ("up", Direction::Up),
        ("down", Direction::Down),
    ] {
        let layout = parse(&format!("layout {{\n    new-pane-direction \"{text}\"\n}}"))
            .layout
            .expect("layout section present");
        assert_eq!(
            layout.new_pane_direction,
            Some(direction),
            "direction {text}"
        );
    }
}

#[test]
fn an_unknown_field_in_each_section_suggests_its_nearest_key() {
    // One near-miss per section, each one edit away from a real key.
    for (source, expected) in [
        (
            "pane {\n    min-col 5\n}",
            "ignored unknown key `pane.min-col`; did you mean `pane.min-cols`?",
        ),
        (
            "layout {\n    new-pane-directio \"down\"\n}",
            "ignored unknown key `layout.new-pane-directio`; did you mean `layout.new-pane-direction`?",
        ),
        (
            "mouse {\n    whel \"ignore\"\n}",
            "ignored unknown key `mouse.whel`; did you mean `mouse.wheel`?",
        ),
        (
            "copy {\n    trim-trailing-whitespac #true\n}",
            "ignored unknown key `copy.trim-trailing-whitespac`; did you mean `copy.trim-trailing-whitespace`?",
        ),
        (
            "terminal {\n    trem \"xterm\"\n}",
            "ignored unknown key `terminal.trem`; did you mean `terminal.term`?",
        ),
        (
            "logging {\n    levl \"info\"\n}",
            "ignored unknown key `logging.levl`; did you mean `logging.level`?",
        ),
    ] {
        let (_, warnings) = parse_with_warnings(source);
        assert_eq!(warnings, vec![expected.to_string()], "source {source:?}");
    }
}

#[test]
fn a_section_written_with_a_value_instead_of_a_block_is_named_in_the_warnings() {
    // `scrollback 5000` reads as a section with no block, so nothing of it is
    // applied. The warning says so rather than leaving the user guessing.
    let (layer, warnings) = parse_with_warnings("scrollback 5000");

    assert_eq!(layer.scrollback, Some(PartialScrollbackConfig::default()));
    assert_eq!(
        warnings,
        vec!["ignored `scrollback` value: a section takes a `{ … }` block".to_string()]
    );
}

#[test]
fn a_section_carrying_both_a_value_and_a_block_reads_the_block_and_names_the_value() {
    let (layer, warnings) = parse_with_warnings("pane 7 { min-cols 12 }");

    assert_eq!(
        layer.pane,
        Some(PartialPaneConfig {
            min_cols: Some(12),
            ..PartialPaneConfig::default()
        })
    );
    assert_eq!(
        warnings,
        vec!["ignored `pane` value: a section takes a `{ … }` block".to_string()]
    );
}
