//! Parser for `koshi.kdl`, the app-settings config file.
//!
//! Turns the top-level sections of `koshi.kdl` into a [`PartialKoshiConfig`]
//! override layer that folds onto the built-in defaults. Does no file I/O: the
//! caller reads the file and hands the text in, as the keybinding parser does.
//!
//! # Field-partial, except `update`
//!
//! Every section but `update` is **field-partial**: a field whose value is the
//! wrong kind is skipped — its default stands — and every other field in the
//! file still applies, so a single typo never reverts a whole file to defaults.
//! Each skipped field is named in the returned warnings for the loader to log.
//!
//! The `update` section is **strict**: a bad field there fails the whole parse.
//! `update.auto-check` gates a network call, and quietly dropping an unreadable
//! `auto-check #false` would re-enable it — so the update loader must fail
//! closed, which needs the parse to fail rather than skip the field.
//!
//! # Example
//! A `koshi.kdl` of
//! ```kdl
//! scrollback {
//!     max-lines 50000
//! }
//! layout {
//!     new-pane-direction "down"
//! }
//! ```
//! yields a layer setting `scrollback.max_lines = 50000` and the default
//! new-pane direction to [`Direction::Down`], leaving every other field at its
//! built-in default.

use std::collections::BTreeSet;
use std::path::Path;

use kdl::KdlNode;
use koshi_core::geometry::Direction;
use koshi_core::log::{LogFormat, LogLevel};

use crate::error::{check_version, ConfigError};
use crate::layer::{
    PartialCopyConfig, PartialKoshiConfig, PartialLayoutDefaults, PartialLoggingConfig,
    PartialMouseConfig, PartialPaneConfig, PartialScrollbackConfig, PartialTerminalConfig,
    PartialUpdateConfig,
};
use crate::parser::{
    parse_kdl, value_bool, value_integer, value_nonempty_string, value_string, value_u16, value_u32,
};
use crate::types::WheelScroll;

/// The section names that may appear at most once, checked for duplicates.
const SECTIONS: &[&str] = &[
    "version",
    "update",
    "pane",
    "scrollback",
    "layout",
    "mouse",
    "copy",
    "terminal",
    "logging",
];

/// Parses `koshi.kdl` `source` into a [`PartialKoshiConfig`] override layer and
/// the warning for every field-partial field that was skipped.
///
/// # Errors
/// Returns [`ConfigError::Parse`] when `source` is not valid KDL, and
/// [`ConfigError::Validation`] for a schema version newer than this build, a
/// duplicate `update` section, or a bad value in the strict `update` section.
pub fn parse_app_config(
    path: &Path,
    source: &str,
) -> Result<(PartialKoshiConfig, Vec<String>), ConfigError> {
    let doc = parse_kdl(path, source)?;
    let mut partial = PartialKoshiConfig::default();
    let mut warnings = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for node in doc.nodes() {
        let name = node.name().value();
        // Each section may appear once. A repeated `version` or `update` is an
        // error (both are strict — a duplicate must never bypass version's
        // newer-schema check or update's fail-closed parse); a repeated
        // field-partial section warns and keeps the first.
        if SECTIONS.contains(&name) && !seen.insert(name) {
            if name == "version" || name == "update" {
                return Err(validation(name, &format!("duplicate `{name}` section")));
            }
            warnings.push(format!("ignored duplicate `{name}` section"));
            continue;
        }
        match name {
            "version" => {
                let found = read_u32(node, "version")?;
                check_version(found).map_err(|diagnostic| ConfigError::Validation {
                    key: "version".to_string(),
                    detail: diagnostic.to_string(),
                })?;
            }
            "update" => partial.update = Some(parse_update(node)?),
            "pane" => partial.pane = Some(parse_pane(node, &mut warnings)),
            "scrollback" => partial.scrollback = Some(parse_scrollback(node, &mut warnings)),
            "layout" => partial.layout = Some(parse_layout_defaults(node, &mut warnings)),
            "mouse" => partial.mouse = Some(parse_mouse(node, &mut warnings)),
            "copy" => partial.copy = Some(parse_copy(node, &mut warnings)),
            "terminal" => partial.terminal = Some(parse_terminal(node, &mut warnings)),
            "logging" => partial.logging = Some(parse_logging(node, &mut warnings)),
            // Unknown top-level sections — a newer build's, or `plugins` until a
            // plugin host exists to consume it — are ignored, not rejected.
            _ => {}
        }
    }
    Ok((partial, warnings))
}

/// Reads the strict `update { … }` block: any bad field fails the whole parse
/// so the update loader can fail closed. Unknown fields are ignored.
fn parse_update(node: &KdlNode) -> Result<PartialUpdateConfig, ConfigError> {
    let mut update = PartialUpdateConfig::default();
    let Some(children) = node.children() else {
        return Ok(update);
    };
    for child in children.nodes() {
        let key = child.name().value();
        match key {
            "auto-check" => update.auto_check = Some(read_bool(child, key)?),
            "check-interval-days" => {
                update.check_interval_days = Some(read_u32(child, key)?);
            }
            "allow-prerelease" => update.allow_prerelease = Some(read_bool(child, key)?),
            _ => {}
        }
    }
    Ok(update)
}

/// Reads the `pane { … }` block.
fn parse_pane(node: &KdlNode, warnings: &mut Vec<String>) -> PartialPaneConfig {
    let mut cfg = PartialPaneConfig::default();
    let Some(children) = node.children() else {
        return cfg;
    };
    for child in children.nodes() {
        let key = child.name().value();
        match key {
            "min-cols" => set(&mut cfg.min_cols, value_u16(child), "pane", key, warnings),
            "min-rows" => set(&mut cfg.min_rows, value_u16(child), "pane", key, warnings),
            other => warnings.push(format!("ignored unknown `pane.{other}`")),
        }
    }
    cfg
}

/// Reads the `scrollback { … }` block.
fn parse_scrollback(node: &KdlNode, warnings: &mut Vec<String>) -> PartialScrollbackConfig {
    let mut cfg = PartialScrollbackConfig::default();
    let Some(children) = node.children() else {
        return cfg;
    };
    for child in children.nodes() {
        let key = child.name().value();
        match key {
            "max-lines" => set(
                &mut cfg.max_lines,
                value_scrollback(child),
                "scrollback",
                key,
                warnings,
            ),
            "max-bytes" => set(
                &mut cfg.max_bytes,
                value_scrollback(child),
                "scrollback",
                key,
                warnings,
            ),
            other => warnings.push(format!("ignored unknown `scrollback.{other}`")),
        }
    }
    cfg
}

/// Reads the `layout { … }` block of default-layout settings.
fn parse_layout_defaults(node: &KdlNode, warnings: &mut Vec<String>) -> PartialLayoutDefaults {
    let mut cfg = PartialLayoutDefaults::default();
    let Some(children) = node.children() else {
        return cfg;
    };
    for child in children.nodes() {
        let key = child.name().value();
        match key {
            "new-pane-direction" => set(
                &mut cfg.new_pane_direction,
                value_direction(child),
                "layout",
                key,
                warnings,
            ),
            other => warnings.push(format!("ignored unknown `layout.{other}`")),
        }
    }
    cfg
}

/// Reads the `mouse { … }` block.
fn parse_mouse(node: &KdlNode, warnings: &mut Vec<String>) -> PartialMouseConfig {
    let mut cfg = PartialMouseConfig::default();
    let Some(children) = node.children() else {
        return cfg;
    };
    for child in children.nodes() {
        let key = child.name().value();
        match key {
            "border-resize" => set(
                &mut cfg.border_resize,
                value_bool(child),
                "mouse",
                key,
                warnings,
            ),
            "scroll-lines" => set(
                &mut cfg.scroll_lines,
                value_u16(child),
                "mouse",
                key,
                warnings,
            ),
            "wheel" => set(&mut cfg.wheel, value_wheel(child), "mouse", key, warnings),
            other => warnings.push(format!("ignored unknown `mouse.{other}`")),
        }
    }
    cfg
}

/// Reads the `copy { … }` block.
fn parse_copy(node: &KdlNode, warnings: &mut Vec<String>) -> PartialCopyConfig {
    let mut cfg = PartialCopyConfig::default();
    let Some(children) = node.children() else {
        return cfg;
    };
    for child in children.nodes() {
        let key = child.name().value();
        match key {
            "trim-trailing-whitespace" => set(
                &mut cfg.trim_trailing_whitespace,
                value_bool(child),
                "copy",
                key,
                warnings,
            ),
            other => warnings.push(format!("ignored unknown `copy.{other}`")),
        }
    }
    cfg
}

/// Reads the `terminal { … }` block.
fn parse_terminal(node: &KdlNode, warnings: &mut Vec<String>) -> PartialTerminalConfig {
    let mut cfg = PartialTerminalConfig::default();
    let Some(children) = node.children() else {
        return cfg;
    };
    for child in children.nodes() {
        let key = child.name().value();
        match key {
            // `term`/`colorterm` are exported to child programs; a blank value
            // (`term ""`) would export an empty `TERM`, which disables terminfo,
            // so it is rejected like any bad field and the default stands.
            "term" => set(
                &mut cfg.term,
                value_nonempty_string(child),
                "terminal",
                key,
                warnings,
            ),
            "colorterm" => set(
                &mut cfg.colorterm,
                value_nonempty_string(child),
                "terminal",
                key,
                warnings,
            ),
            // `default-shell` is `Option<Option<String>>`: the outer layer marks
            // it set, the inner is the shell (there is no "unset it to $SHELL"
            // spelling in the file, only "name a shell"). A blank value would
            // spawn an empty program, so it is rejected and `$SHELL` stands.
            "default-shell" => set(
                &mut cfg.default_shell,
                value_nonempty_string(child).map(Some),
                "terminal",
                key,
                warnings,
            ),
            other => warnings.push(format!("ignored unknown `terminal.{other}`")),
        }
    }
    cfg
}

/// Reads the `logging { … }` block.
fn parse_logging(node: &KdlNode, warnings: &mut Vec<String>) -> PartialLoggingConfig {
    let mut cfg = PartialLoggingConfig::default();
    let Some(children) = node.children() else {
        return cfg;
    };
    for child in children.nodes() {
        let key = child.name().value();
        match key {
            "enabled" => set(
                &mut cfg.enabled,
                value_bool(child),
                "logging",
                key,
                warnings,
            ),
            "level" => set(
                &mut cfg.level,
                value_log_level(child),
                "logging",
                key,
                warnings,
            ),
            "format" => set(
                &mut cfg.format,
                value_log_format(child),
                "logging",
                key,
                warnings,
            ),
            other => warnings.push(format!("ignored unknown `logging.{other}`")),
        }
    }
    cfg
}

/// Stores a parsed field-partial value, or records a warning naming the field
/// and the reason it was skipped.
fn set<T>(
    slot: &mut Option<T>,
    parsed: Result<T, String>,
    section: &str,
    key: &str,
    warnings: &mut Vec<String>,
) {
    match parsed {
        Ok(value) => *slot = Some(value),
        Err(detail) => warnings.push(format!("ignored `{section}.{key}`: {detail}")),
    }
}

/// Reads a scrollback cap. A negative value is clamped to `0` — "no
/// scrollback": the buffer keeps nothing and lines drop as they scroll off,
/// rather than being rejected as a bad field.
fn value_scrollback(node: &KdlNode) -> Result<usize, String> {
    Ok(value_integer(node)?.clamp(0, usize::MAX as i128) as usize)
}

/// Reads the node's single value as a split [`Direction`].
fn value_direction(node: &KdlNode) -> Result<Direction, String> {
    match value_string(node)?.as_str() {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        _ => Err(r#"expected "left", "right", "up", or "down""#.to_string()),
    }
}

/// Reads the node's single value as a [`LogLevel`] — the lowest severity that
/// gets written to the log file.
fn value_log_level(node: &KdlNode) -> Result<LogLevel, String> {
    match value_string(node)?.as_str() {
        "info" => Ok(LogLevel::Info),
        "warning" => Ok(LogLevel::Warning),
        "error" => Ok(LogLevel::Error),
        _ => Err(r#"expected "info", "warning", or "error""#.to_string()),
    }
}

/// Reads the node's single value as a [`LogFormat`] — how each written line is
/// rendered.
fn value_log_format(node: &KdlNode) -> Result<LogFormat, String> {
    match value_string(node)?.as_str() {
        "pretty" => Ok(LogFormat::Pretty),
        "json" => Ok(LogFormat::Json),
        _ => Err(r#"expected "pretty" or "json""#.to_string()),
    }
}

/// Reads the node's single value as a [`WheelScroll`] behavior.
fn value_wheel(node: &KdlNode) -> Result<WheelScroll, String> {
    match value_string(node)?.as_str() {
        "scroll-scrollback" => Ok(WheelScroll::ScrollScrollback),
        "ignore" => Ok(WheelScroll::Ignore),
        _ => Err(r#"expected "scroll-scrollback" or "ignore""#.to_string()),
    }
}

/// Reads the node's single value as a boolean for the strict `update` section.
fn read_bool(node: &KdlNode, key: &str) -> Result<bool, ConfigError> {
    value_bool(node).map_err(|detail| validation(key, &detail))
}

/// Reads the node's single value as a `u32` for the strict `update` section.
fn read_u32(node: &KdlNode, key: &str) -> Result<u32, ConfigError> {
    value_u32(node).map_err(|detail| validation(key, &detail))
}

/// Builds a [`ConfigError::Validation`] for a bad strict-section field value.
fn validation(key: &str, detail: &str) -> ConfigError {
    ConfigError::Validation {
        key: key.to_string(),
        detail: detail.to_string(),
    }
}

#[cfg(test)]
mod tests;
