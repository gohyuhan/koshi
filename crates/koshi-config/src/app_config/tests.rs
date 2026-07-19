//! Tests for the `koshi.kdl` app-config parser.

use std::path::Path;

use crate::error::ConfigError;

use super::parse_app_config;

/// Parses `source` as `koshi.kdl`, panicking on error.
fn parse(source: &str) -> crate::layer::PartialKoshiConfig {
    parse_app_config(Path::new("koshi.kdl"), source).expect("valid config")
}

#[test]
fn empty_source_sets_no_layer() {
    let layer = parse("");
    assert_eq!(layer.update, None);
}

#[test]
fn reads_both_update_fields() {
    let update = parse("update {\n    auto-check #false\n    check-interval-days 30\n}")
        .update
        .expect("update section present");
    assert_eq!(update.auto_check, Some(false));
    assert_eq!(update.check_interval_days, Some(30));
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
fn unknown_top_level_node_is_ignored() {
    let layer = parse("mouse {\n    scroll-lines 5\n}");
    assert_eq!(layer.update, None);
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
fn syntax_error_is_a_parse_error() {
    let error = parse_app_config(Path::new("koshi.kdl"), "update { auto-check #true")
        .expect_err("unclosed block");
    assert!(matches!(error, ConfigError::Parse { .. }));
}
