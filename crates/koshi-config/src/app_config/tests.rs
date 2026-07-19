//! Tests for the `koshi.kdl` app-config parser.

use std::path::Path;

use koshi_core::geometry::Direction;
use koshi_core::log::{LogFormat, LogLevel};

use crate::error::ConfigError;
use crate::layer::PartialKoshiConfig;
use crate::types::WheelScroll;

use super::parse_app_config;

/// Parses `source` as `koshi.kdl`, panicking on error, dropping warnings.
fn parse(source: &str) -> PartialKoshiConfig {
    parse_app_config(Path::new("koshi.kdl"), source)
        .expect("valid config")
        .0
}

/// Parses `source`, returning both the layer and the field-partial warnings.
fn parse_with_warnings(source: &str) -> (PartialKoshiConfig, Vec<String>) {
    parse_app_config(Path::new("koshi.kdl"), source).expect("valid config")
}

#[test]
fn empty_source_sets_no_layer() {
    let layer = parse("");
    assert_eq!(layer.update, None);
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
    assert!(matches!(
        error,
        ConfigError::Validation { key, .. } if key == "allow-prerelease"
    ));
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
fn an_unknown_top_level_node_is_ignored() {
    let (layer, warnings) = parse_with_warnings("frobnicate {\n    whatever 5\n}");
    assert_eq!(layer.update, None);
    assert_eq!(layer.mouse, None);
    assert!(
        warnings.is_empty(),
        "an unknown section is silent, not a warning"
    );
}

#[test]
fn unknown_field_inside_update_is_ignored() {
    let update = parse("update {\n    auto-check #true\n    frequency 5\n}")
        .update
        .expect("update section present");
    assert_eq!(update.auto_check, Some(true));
    assert_eq!(update.check_interval_days, None);
}

#[test]
fn non_boolean_auto_check_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    auto-check \"yes\"\n}",
    )
    .expect_err("string is not a boolean");
    assert!(matches!(
        error,
        ConfigError::Validation { key, .. } if key == "auto-check"
    ));
}

#[test]
fn non_integer_interval_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    check-interval-days #true\n}",
    )
    .expect_err("boolean is not an integer");
    assert!(matches!(
        error,
        ConfigError::Validation { key, .. } if key == "check-interval-days"
    ));
}

#[test]
fn negative_interval_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    check-interval-days -3\n}",
    )
    .expect_err("negative does not fit u32");
    assert!(matches!(error, ConfigError::Validation { key, .. } if key == "check-interval-days"));
}

#[test]
fn extra_argument_on_a_field_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    check-interval-days 3 9\n}",
    )
    .expect_err("two values is not one");
    assert!(matches!(error, ConfigError::Validation { .. }));
}

#[test]
fn a_duplicate_update_section_is_a_validation_error() {
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "update {\n    auto-check #true\n}\nupdate {\n    auto-check #false\n}",
    )
    .expect_err("two update sections");
    assert!(matches!(error, ConfigError::Validation { key, .. } if key == "update"));
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
    assert!(matches!(error, ConfigError::Validation { key, .. } if key == "version"));
}

#[test]
fn a_duplicate_version_is_a_hard_error() {
    // A second `version` must not be treated as a skippable duplicate section:
    // that would `continue` before the newer-schema check runs and let a config
    // declared for a newer build apply. It fails closed instead.
    let error = parse_app_config(
        Path::new("koshi.kdl"),
        "version 1\nversion 999\npane {\n    min-cols 5\n}",
    )
    .expect_err("duplicate version rejected");
    assert!(matches!(error, ConfigError::Validation { key, .. } if key == "version"));
}

#[test]
fn syntax_error_is_a_parse_error() {
    let error = parse_app_config(Path::new("koshi.kdl"), "update { auto-check #true")
        .expect_err("unclosed block");
    assert!(matches!(error, ConfigError::Parse { .. }));
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
    // An empty `TERM` disables terminfo and an empty `COLORTERM` is meaningless;
    // both (including whitespace-only) are dropped like any bad field, so the
    // built-in `xterm-256color`/`truecolor` identity applies.
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
    // An empty (or whitespace-only) `default-shell` would spawn an empty
    // program; it is dropped like any bad field, so `default_shell` stays unset
    // and the spawn path falls back to `$SHELL`/`%COMSPEC%`.
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
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].contains("logging.level"),
        "warning names the field: {}",
        warnings[0]
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
    // A negative cap is clamped to 0 ("no scrollback"), not rejected.
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
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("layout.new-pane-direction"));
}

#[test]
fn an_unknown_field_in_a_section_warns() {
    let (_, warnings) = parse_with_warnings("scrollback {\n    frequency 5\n}");
    assert_eq!(
        warnings,
        vec!["ignored unknown `scrollback.frequency`".to_string()]
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
