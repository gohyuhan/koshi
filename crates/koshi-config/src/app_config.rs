//! Parser for `koshi.kdl`, the app-settings config file.
//!
//! Turns the top-level sections of `koshi.kdl` into a [`PartialKoshiConfig`]
//! override layer that folds onto the built-in defaults. Does no file I/O: the
//! caller reads the file and hands the text in.
//!
//! # Field-partial, except `update`
//!
//! Every section but `update` is **field-partial**: a field whose value is the
//! wrong kind is skipped, its default stands, and every other field in the
//! file still applies. Each skipped field is named in the returned warnings.
//!
//! The `update` section is **strict**: a field there whose value is the wrong
//! kind fails the whole parse.
//!
//! # The `theme` line
//!
//! `theme "midnight"` names which color theme to use; the colors live in a
//! separate `themes/midnight.kdl`. This parser records only the name and
//! returns it beside the layer, not inside it. See [`AppConfigFile`].
//!
//! # Example
//! A `koshi.kdl` of
//! ```kdl
//! version 2
//! theme "midnight"
//! scrollback {
//!     max-lines 50000
//! }
//! layout {
//!     new-pane-direction "down"
//! }
//! ```
//! yields `theme = Some("midnight")` and a layer setting
//! `scrollback.maximum_line_count = 50000` and the default new-pane direction to
//! [`Direction::Down`], leaving every other field at its built-in default.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use kdl::KdlNode;
use koshi_core::geometry::Direction;
use koshi_core::key::ExtendedKeysMode;
use koshi_core::log::{LogFormat, LogLevel};

use crate::error::{build_validation_error, validate_config_schema_version, ConfigError};
use crate::layer::{
    PartialCopyConfig, PartialKoshiConfig, PartialLayoutDefaults, PartialLoggingConfig,
    PartialMouseConfig, PartialPaneConfig, PartialScrollbackConfig, PartialTerminalConfig,
    PartialUpdateConfig,
};
use crate::parser::{
    find_section_block, format_unknown_key, parse_boolean_kdl_value, parse_integer_kdl_value,
    parse_kdl, parse_nonempty_string_kdl_value, parse_string_kdl_value, parse_u16_kdl_value,
    parse_u32_kdl_value, parse_version_argument, set_parsed_field,
};
use crate::types::WheelScroll;

/// The top-level node names. Each may appear at most once; an unknown name is
/// matched against these for the `did you mean` hint.
const APP_CONFIG_SECTION_NAMES: &[&str] = &[
    "version",
    "update",
    "theme",
    "pane",
    "scrollback",
    "layout",
    "mouse",
    "copy",
    "terminal",
    "logging",
    "image-support",
    "remote-reconnect",
    "allow-beta-features",
    "allow-other-users",
    "remote-listen",
    "shared-sessions-dir",
    "auto-close-session",
];

/// A parsed `koshi.kdl`.
///
/// The theme name is kept **out** of [`layer`](Self::layer): `layer.theme` is
/// always `None`, and the name from the `theme` line is in
/// [`theme_name`](Self::theme_name).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AppConfigFile {
    /// The settings this file overrides, to fold onto the built-in defaults.
    pub layer: PartialKoshiConfig,
    /// The name from the `theme "<name>"` line, trimmed of surrounding
    /// whitespace; `themes/<name>.kdl` supplies the colors. `None` when the
    /// file names no theme.
    pub theme_name: Option<String>,
    /// One warning per skipped field, skipped duplicate section, and unknown
    /// key, in file order.
    pub parse_warnings: Vec<String>,
}

/// Parses `koshi.kdl` `source` into its override layer, the theme it names, and
/// one warning per skipped field, skipped duplicate section, and unknown key.
///
/// # Errors
/// Returns [`ConfigError::Parse`] when `source` is not valid KDL.
///
/// Returns [`ConfigError::Validation`] when `version` is missing, repeated,
/// carries a `{ … }` block, is not a single integer from `0` to `4294967295`,
/// is `0`, or is newer than this build supports; when `update` is repeated; or
/// when an `update` field is not a single value of the right type and range.
pub fn parse_app_config(
    config_path: &Path,
    config_source_text: &str,
) -> Result<AppConfigFile, ConfigError> {
    let config_document = parse_kdl(config_path, config_source_text)?;
    let mut partial_koshi_config = PartialKoshiConfig::default();
    let mut theme_name = None;
    let mut parse_warnings = Vec::new();
    let mut seen_config_section_names: BTreeSet<&str> = BTreeSet::new();
    for config_node in config_document.nodes() {
        let config_section_name = config_node.name().value();
        // Each section may appear once. A repeated `version` or `update` is an
        // error; a repeated field-partial section is dropped with a warning
        // and the first stands.
        if APP_CONFIG_SECTION_NAMES.contains(&config_section_name)
            && !seen_config_section_names.insert(config_section_name)
        {
            if config_section_name == "version" || config_section_name == "update" {
                return Err(build_validation_error(
                    config_section_name,
                    &format!("`{config_section_name}` is declared more than once"),
                ));
            }
            parse_warnings.push(format!("ignored duplicate `{config_section_name}` section"));
            continue;
        }
        match config_section_name {
            "version" => {
                let declared_version =
                    parse_version_argument(config_node).map_err(|(_, version_error_detail)| {
                        build_validation_error("version", version_error_detail)
                    })?;
                validate_config_schema_version(declared_version).map_err(|diagnostic| {
                    build_validation_error("version", &diagnostic.to_string())
                })?;
            }
            // `theme` names which `themes/<name>.kdl` supplies the colors; the
            // colors themselves are never spelled here.
            "theme" => set_top_level_field(
                &mut theme_name,
                parse_nonempty_string_kdl_value(config_node),
                config_section_name,
                &mut parse_warnings,
            ),
            "update" => {
                partial_koshi_config.update =
                    Some(parse_update_config(config_node, &mut parse_warnings)?);
            }
            "pane" => {
                partial_koshi_config.pane =
                    Some(parse_pane_config(config_node, &mut parse_warnings));
            }
            "scrollback" => {
                partial_koshi_config.scrollback =
                    Some(parse_scrollback_config(config_node, &mut parse_warnings));
            }
            "layout" => {
                partial_koshi_config.layout =
                    Some(parse_layout_defaults(config_node, &mut parse_warnings));
            }
            "mouse" => {
                partial_koshi_config.mouse =
                    Some(parse_mouse_config(config_node, &mut parse_warnings));
            }
            "copy" => {
                partial_koshi_config.copy =
                    Some(parse_copy_config(config_node, &mut parse_warnings));
            }
            "terminal" => {
                partial_koshi_config.terminal =
                    Some(parse_terminal_config(config_node, &mut parse_warnings));
            }
            "logging" => {
                partial_koshi_config.logging =
                    Some(parse_logging_config(config_node, &mut parse_warnings));
            }
            "image-support" => set_top_level_field(
                &mut partial_koshi_config.supports_image_protocols,
                parse_boolean_kdl_value(config_node),
                config_section_name,
                &mut parse_warnings,
            ),
            "remote-reconnect" => set_top_level_field(
                &mut partial_koshi_config.should_reconnect_remote_session,
                parse_boolean_kdl_value(config_node),
                config_section_name,
                &mut parse_warnings,
            ),
            "allow-beta-features" => set_top_level_field(
                &mut partial_koshi_config.should_allow_beta_features,
                parse_boolean_kdl_value(config_node),
                config_section_name,
                &mut parse_warnings,
            ),
            "allow-other-users" => set_top_level_field(
                &mut partial_koshi_config.should_allow_other_users,
                parse_boolean_kdl_value(config_node),
                config_section_name,
                &mut parse_warnings,
            ),
            // `remote-listen` is `Option<Option<String>>`: the outer layer
            // marks the field set, the inner carries the address.
            "remote-listen" => set_top_level_field(
                &mut partial_koshi_config.remote_listen,
                parse_nonempty_string_kdl_value(config_node).map(Some),
                config_section_name,
                &mut parse_warnings,
            ),
            // `shared-sessions-dir` is `Option<Option<PathBuf>>`: the outer
            // layer marks the field set, the inner carries the directory.
            "shared-sessions-dir" => set_top_level_field(
                &mut partial_koshi_config.shared_sessions_directory,
                parse_nonempty_string_kdl_value(config_node)
                    .map(|directory_path| Some(PathBuf::from(directory_path))),
                config_section_name,
                &mut parse_warnings,
            ),
            "auto-close-session" => set_top_level_field(
                &mut partial_koshi_config.should_auto_close_session,
                parse_boolean_kdl_value(config_node),
                config_section_name,
                &mut parse_warnings,
            ),
            unknown_section_name => parse_warnings.push(format!(
                "ignored {}",
                format_unknown_key(unknown_section_name, APP_CONFIG_SECTION_NAMES)
            )),
        }
    }
    if !seen_config_section_names.contains("version") {
        return Err(build_validation_error(
            "version",
            "file must declare `version`",
        ));
    }
    Ok(AppConfigFile {
        layer: partial_koshi_config,
        theme_name,
        parse_warnings,
    })
}

/// Stores a parsed top-level field in `parsed_value_slot`. On `Err`, leaves `parsed_value_slot`
/// untouched and pushes one warning naming the field and the reason.
///
/// `field_name` is the top-level node's name (`remote-listen`). A `parsed_value` of
/// `Err("must not be empty")` pushes ``ignored `remote-listen`: must not be
/// empty``.
fn set_top_level_field<FieldValue>(
    field_value_slot: &mut Option<FieldValue>,
    field_parse_result: Result<FieldValue, String>,
    field_name: &str,
    parse_warnings: &mut Vec<String>,
) {
    match field_parse_result {
        Ok(parsed_field_value) => *field_value_slot = Some(parsed_field_value),
        Err(parse_error_detail) => {
            parse_warnings.push(format!("ignored `{field_name}`: {parse_error_detail}"));
        }
    }
}

/// Reads the strict `update { … }` block. A field whose value is the wrong
/// kind fails the whole parse; an unknown field is dropped with a warning.
fn parse_update_config(
    config_node: &KdlNode,
    parse_warnings: &mut Vec<String>,
) -> Result<PartialUpdateConfig, ConfigError> {
    let mut partial_update_config = PartialUpdateConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_warnings) else {
        return Ok(partial_update_config);
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "auto-check" => {
                partial_update_config.should_auto_check_for_updates =
                    Some(parse_required_boolean_field(field_node, field_name)?);
            }
            "check-interval-days" => {
                partial_update_config.check_interval_days =
                    Some(parse_required_u32_field(field_node, field_name)?);
            }
            "allow-prerelease" => {
                partial_update_config.should_allow_prerelease_updates =
                    Some(parse_required_boolean_field(field_node, field_name)?);
            }
            unknown_field_name => parse_warnings.push(format!(
                "ignored {}",
                format_unknown_key(
                    &format!("update.{unknown_field_name}"),
                    &[
                        "update.auto-check",
                        "update.check-interval-days",
                        "update.allow-prerelease",
                    ],
                )
            )),
        }
    }
    Ok(partial_update_config)
}

/// Reads the `pane { … }` block.
fn parse_pane_config(config_node: &KdlNode, parse_warnings: &mut Vec<String>) -> PartialPaneConfig {
    let mut partial_pane_config = PartialPaneConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_warnings) else {
        return partial_pane_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "min-cols" => set_parsed_field(
                &mut partial_pane_config.minimum_column_count,
                parse_u16_kdl_value(field_node),
                "pane",
                field_name,
                parse_warnings,
            ),
            "min-rows" => set_parsed_field(
                &mut partial_pane_config.minimum_row_count,
                parse_u16_kdl_value(field_node),
                "pane",
                field_name,
                parse_warnings,
            ),
            "gap" => set_parsed_field(
                &mut partial_pane_config.gap_cell_count,
                parse_u16_kdl_value(field_node),
                "pane",
                field_name,
                parse_warnings,
            ),
            unknown_field_name => parse_warnings.push(format!(
                "ignored {}",
                format_unknown_key(
                    &format!("pane.{unknown_field_name}"),
                    &["pane.min-cols", "pane.min-rows", "pane.gap"],
                )
            )),
        }
    }
    partial_pane_config
}

/// Reads the `scrollback { … }` block.
fn parse_scrollback_config(
    config_node: &KdlNode,
    parse_warnings: &mut Vec<String>,
) -> PartialScrollbackConfig {
    let mut partial_scrollback_config = PartialScrollbackConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_warnings) else {
        return partial_scrollback_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "max-lines" => set_parsed_field(
                &mut partial_scrollback_config.maximum_line_count,
                parse_scrollback_limit(field_node),
                "scrollback",
                field_name,
                parse_warnings,
            ),
            "max-bytes" => set_parsed_field(
                &mut partial_scrollback_config.maximum_byte_count,
                parse_scrollback_limit(field_node),
                "scrollback",
                field_name,
                parse_warnings,
            ),
            "scroll-on-input" => set_parsed_field(
                &mut partial_scrollback_config.should_scroll_to_input,
                parse_boolean_kdl_value(field_node),
                "scrollback",
                field_name,
                parse_warnings,
            ),
            unknown_field_name => parse_warnings.push(format!(
                "ignored {}",
                format_unknown_key(
                    &format!("scrollback.{unknown_field_name}"),
                    &[
                        "scrollback.max-lines",
                        "scrollback.max-bytes",
                        "scrollback.scroll-on-input",
                    ],
                )
            )),
        }
    }
    partial_scrollback_config
}

/// Reads the `layout { … }` block of default-layout settings.
fn parse_layout_defaults(
    config_node: &KdlNode,
    parse_warnings: &mut Vec<String>,
) -> PartialLayoutDefaults {
    let mut partial_layout_defaults = PartialLayoutDefaults::default();
    let Some(section_children) = find_section_block(config_node, parse_warnings) else {
        return partial_layout_defaults;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "new-pane-direction" => set_parsed_field(
                &mut partial_layout_defaults.new_pane_direction,
                parse_direction(field_node),
                "layout",
                field_name,
                parse_warnings,
            ),
            unknown_field_name => parse_warnings.push(format!(
                "ignored {}",
                format_unknown_key(
                    &format!("layout.{unknown_field_name}"),
                    &["layout.new-pane-direction"],
                )
            )),
        }
    }
    partial_layout_defaults
}

/// Reads the `mouse { … }` block.
fn parse_mouse_config(
    config_node: &KdlNode,
    parse_warnings: &mut Vec<String>,
) -> PartialMouseConfig {
    let mut partial_mouse_config = PartialMouseConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_warnings) else {
        return partial_mouse_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "border-resize" => set_parsed_field(
                &mut partial_mouse_config.can_resize_pane_border,
                parse_boolean_kdl_value(field_node),
                "mouse",
                field_name,
                parse_warnings,
            ),
            "scroll-lines" => set_parsed_field(
                &mut partial_mouse_config.scroll_line_count,
                parse_u16_kdl_value(field_node),
                "mouse",
                field_name,
                parse_warnings,
            ),
            "wheel" => set_parsed_field(
                &mut partial_mouse_config.wheel,
                parse_wheel_scroll(field_node),
                "mouse",
                field_name,
                parse_warnings,
            ),
            unknown_field_name => parse_warnings.push(format!(
                "ignored {}",
                format_unknown_key(
                    &format!("mouse.{unknown_field_name}"),
                    &["mouse.border-resize", "mouse.scroll-lines", "mouse.wheel",],
                )
            )),
        }
    }
    partial_mouse_config
}

/// Reads the `copy { … }` block.
fn parse_copy_config(config_node: &KdlNode, parse_warnings: &mut Vec<String>) -> PartialCopyConfig {
    let mut partial_copy_config = PartialCopyConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_warnings) else {
        return partial_copy_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "trim-trailing-whitespace" => set_parsed_field(
                &mut partial_copy_config.should_trim_trailing_whitespace,
                parse_boolean_kdl_value(field_node),
                "copy",
                field_name,
                parse_warnings,
            ),
            unknown_field_name => parse_warnings.push(format!(
                "ignored {}",
                format_unknown_key(
                    &format!("copy.{unknown_field_name}"),
                    &["copy.trim-trailing-whitespace"],
                )
            )),
        }
    }
    partial_copy_config
}

/// Reads the `terminal { … }` block.
fn parse_terminal_config(
    config_node: &KdlNode,
    parse_warnings: &mut Vec<String>,
) -> PartialTerminalConfig {
    let mut partial_terminal_config = PartialTerminalConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_warnings) else {
        return partial_terminal_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            // A blank or whitespace-only `term`/`colorterm` is dropped with a
            // warning.
            "term" => set_parsed_field(
                &mut partial_terminal_config.term,
                parse_nonempty_string_kdl_value(field_node),
                "terminal",
                field_name,
                parse_warnings,
            ),
            "colorterm" => set_parsed_field(
                &mut partial_terminal_config.colorterm,
                parse_nonempty_string_kdl_value(field_node),
                "terminal",
                field_name,
                parse_warnings,
            ),
            // `default-shell` is `Option<Option<String>>`: the outer layer marks
            // it set, the inner is the shell. The file can only name a shell;
            // it cannot unset one. A blank value is dropped with a warning.
            "default-shell" => set_parsed_field(
                &mut partial_terminal_config.default_shell,
                parse_nonempty_string_kdl_value(field_node).map(Some),
                "terminal",
                field_name,
                parse_warnings,
            ),
            // `extended-keys` decides what a pane that pushed no Kitty
            // keyboard flag receives for a key whose legacy bytes another key
            // also owns.
            "extended-keys" => set_parsed_field(
                &mut partial_terminal_config.extended_keys_mode,
                parse_extended_keys_mode(field_node),
                "terminal",
                field_name,
                parse_warnings,
            ),
            unknown_field_name => parse_warnings.push(format!(
                "ignored {}",
                format_unknown_key(
                    &format!("terminal.{unknown_field_name}"),
                    &[
                        "terminal.term",
                        "terminal.colorterm",
                        "terminal.default-shell",
                        "terminal.extended-keys",
                    ],
                )
            )),
        }
    }
    partial_terminal_config
}

/// Reads the `logging { … }` block.
fn parse_logging_config(
    config_node: &KdlNode,
    parse_warnings: &mut Vec<String>,
) -> PartialLoggingConfig {
    let mut partial_logging_config = PartialLoggingConfig::default();
    let Some(section_children) = find_section_block(config_node, parse_warnings) else {
        return partial_logging_config;
    };
    for field_node in section_children.nodes() {
        let field_name = field_node.name().value();
        match field_name {
            "enabled" => set_parsed_field(
                &mut partial_logging_config.is_enabled,
                parse_boolean_kdl_value(field_node),
                "logging",
                field_name,
                parse_warnings,
            ),
            "level" => set_parsed_field(
                &mut partial_logging_config.level,
                parse_log_level(field_node),
                "logging",
                field_name,
                parse_warnings,
            ),
            "format" => set_parsed_field(
                &mut partial_logging_config.log_format,
                parse_log_format(field_node),
                "logging",
                field_name,
                parse_warnings,
            ),
            unknown_field_name => parse_warnings.push(format!(
                "ignored {}",
                format_unknown_key(
                    &format!("logging.{unknown_field_name}"),
                    &["logging.enabled", "logging.level", "logging.format"],
                )
            )),
        }
    }
    partial_logging_config
}

/// Reads a scrollback cap. A negative value becomes `0` (no scrollback); a
/// value above `usize::MAX` becomes `usize::MAX`. `max-lines -5` yields `0`.
fn parse_scrollback_limit(field_node: &KdlNode) -> Result<usize, String> {
    Ok(parse_integer_kdl_value(field_node)?.clamp(0, usize::MAX as i128) as usize)
}

/// Reads the node's single value as a split [`Direction`].
fn parse_direction(field_node: &KdlNode) -> Result<Direction, String> {
    match parse_string_kdl_value(field_node)? {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        _ => Err(r#"expected "left", "right", "up", or "down""#.to_string()),
    }
}

/// Reads the node's single value as a [`LogLevel`], the lowest severity that
/// is written to the log file.
fn parse_log_level(field_node: &KdlNode) -> Result<LogLevel, String> {
    match parse_string_kdl_value(field_node)? {
        "info" => Ok(LogLevel::Info),
        "warning" => Ok(LogLevel::Warning),
        "error" => Ok(LogLevel::Error),
        _ => Err(r#"expected "info", "warning", or "error""#.to_string()),
    }
}

/// Reads the node's single value as a [`LogFormat`], the shape of each written
/// log line.
fn parse_log_format(field_node: &KdlNode) -> Result<LogFormat, String> {
    match parse_string_kdl_value(field_node)? {
        "pretty" => Ok(LogFormat::Pretty),
        "json" => Ok(LogFormat::Json),
        _ => Err(r#"expected "pretty" or "json""#.to_string()),
    }
}

/// Reads the node's single value as an [`ExtendedKeysMode`].
fn parse_extended_keys_mode(field_node: &KdlNode) -> Result<ExtendedKeysMode, String> {
    match parse_string_kdl_value(field_node)? {
        "on-request" => Ok(ExtendedKeysMode::OnRequest),
        "always" => Ok(ExtendedKeysMode::Always),
        _ => Err(r#"expected "on-request" or "always""#.to_string()),
    }
}

/// Reads the node's single value as a [`WheelScroll`] behavior.
fn parse_wheel_scroll(field_node: &KdlNode) -> Result<WheelScroll, String> {
    match parse_string_kdl_value(field_node)? {
        "scroll-scrollback" => Ok(WheelScroll::ScrollScrollback),
        "ignore" => Ok(WheelScroll::Ignore),
        _ => Err(r#"expected "scroll-scrollback" or "ignore""#.to_string()),
    }
}

/// Reads the node's single value as a boolean for the strict `update` section.
fn parse_required_boolean_field(
    field_node: &KdlNode,
    field_name: &str,
) -> Result<bool, ConfigError> {
    parse_boolean_kdl_value(field_node)
        .map_err(|parse_error_detail| build_validation_error(field_name, &parse_error_detail))
}

/// Reads the node's single value as a `u32` for the strict `update` section.
fn parse_required_u32_field(field_node: &KdlNode, field_name: &str) -> Result<u32, ConfigError> {
    parse_u32_kdl_value(field_node)
        .map_err(|parse_error_detail| build_validation_error(field_name, &parse_error_detail))
}

#[cfg(test)]
mod tests;
