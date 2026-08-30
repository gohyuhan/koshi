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
//! version 1
//! theme "midnight"
//! scrollback {
//!     max-lines 50000
//! }
//! layout {
//!     new-pane-direction "down"
//! }
//! ```
//! yields `theme = Some("midnight")` and a layer setting
//! `scrollback.max_lines = 50000` and the default new-pane direction to
//! [`Direction::Down`], leaving every other field at its built-in default.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use kdl::KdlNode;
use koshi_core::geometry::Direction;
use koshi_core::log::{LogFormat, LogLevel};

use crate::error::{check_version, validation, ConfigError};
use crate::layer::{
    PartialCopyConfig, PartialKoshiConfig, PartialLayoutDefaults, PartialLoggingConfig,
    PartialMouseConfig, PartialPaneConfig, PartialScrollbackConfig, PartialTerminalConfig,
    PartialUpdateConfig,
};
use crate::parser::{
    parse_kdl, set, unknown_key, value_bool, value_integer, value_nonempty_string, value_string,
    value_u16, value_u32,
};
use crate::types::WheelScroll;

/// The top-level node names. Each may appear at most once; an unknown name is
/// matched against these for the `did you mean` hint.
const SECTIONS: &[&str] = &[
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
/// [`theme`](Self::theme).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AppConfigFile {
    /// The settings this file overrides, to fold onto the built-in defaults.
    pub layer: PartialKoshiConfig,
    /// The name from the `theme "<name>"` line, trimmed of surrounding
    /// whitespace; `themes/<name>.kdl` supplies the colors. `None` when the
    /// file names no theme.
    pub theme: Option<String>,
    /// One entry per skipped field, skipped duplicate section, and unknown
    /// key, in file order.
    pub warnings: Vec<String>,
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
pub fn parse_app_config(path: &Path, source: &str) -> Result<AppConfigFile, ConfigError> {
    let doc = parse_kdl(path, source)?;
    let mut partial = PartialKoshiConfig::default();
    let mut theme = None;
    let mut warnings = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for node in doc.nodes() {
        let name = node.name().value();
        // Each section may appear once. A repeated `version` or `update` is an
        // error; a repeated field-partial section is dropped with a warning
        // and the first stands.
        if SECTIONS.contains(&name) && !seen.insert(name) {
            if name == "version" || name == "update" {
                return Err(validation(name, &format!("duplicate `{name}` section")));
            }
            warnings.push(format!("ignored duplicate `{name}` section"));
            continue;
        }
        match name {
            "version" => {
                if node.children().is_some() {
                    return Err(validation("version", "`version` takes no children"));
                }
                let found = read_u32(node, "version")?;
                check_version(found)
                    .map_err(|diagnostic| validation("version", &diagnostic.to_string()))?;
            }
            // `theme` names which `themes/<name>.kdl` supplies the colors; the
            // colors themselves are never spelled here.
            "theme" => set_top_level(&mut theme, value_nonempty_string(node), name, &mut warnings),
            "update" => partial.update = Some(parse_update(node, &mut warnings)?),
            "pane" => partial.pane = Some(parse_pane(node, &mut warnings)),
            "scrollback" => partial.scrollback = Some(parse_scrollback(node, &mut warnings)),
            "layout" => partial.layout = Some(parse_layout_defaults(node, &mut warnings)),
            "mouse" => partial.mouse = Some(parse_mouse(node, &mut warnings)),
            "copy" => partial.copy = Some(parse_copy(node, &mut warnings)),
            "terminal" => partial.terminal = Some(parse_terminal(node, &mut warnings)),
            "logging" => partial.logging = Some(parse_logging(node, &mut warnings)),
            "remote-reconnect" => set_top_level(
                &mut partial.remote_reconnect,
                value_bool(node),
                name,
                &mut warnings,
            ),
            "allow-beta-features" => set_top_level(
                &mut partial.allow_beta_features,
                value_bool(node),
                name,
                &mut warnings,
            ),
            "allow-other-users" => set_top_level(
                &mut partial.allow_other_users,
                value_bool(node),
                name,
                &mut warnings,
            ),
            // `remote-listen` is `Option<Option<String>>`: the outer layer
            // marks the field set, the inner carries the address.
            "remote-listen" => set_top_level(
                &mut partial.remote_listen,
                value_nonempty_string(node).map(Some),
                name,
                &mut warnings,
            ),
            // `shared-sessions-dir` is `Option<Option<PathBuf>>`: the outer
            // layer marks the field set, the inner carries the directory.
            "shared-sessions-dir" => set_top_level(
                &mut partial.shared_sessions_dir,
                value_nonempty_string(node).map(|dir| Some(PathBuf::from(dir))),
                name,
                &mut warnings,
            ),
            "auto-close-session" => set_top_level(
                &mut partial.auto_close_session,
                value_bool(node),
                name,
                &mut warnings,
            ),
            other => warnings.push(format!("ignored {}", unknown_key(other, SECTIONS))),
        }
    }
    if !seen.contains("version") {
        return Err(validation("version", "file must declare `version`"));
    }
    Ok(AppConfigFile {
        layer: partial,
        theme,
        warnings,
    })
}

/// Stores a parsed top-level field in `slot`. On `Err`, leaves `slot`
/// untouched and pushes one warning naming the field and the reason.
///
/// `key` is the top-level node's name (`remote-listen`). A `parsed` of
/// `Err("must not be empty")` pushes ``ignored `remote-listen`: must not be
/// empty``.
fn set_top_level<T>(
    slot: &mut Option<T>,
    parsed: Result<T, String>,
    key: &str,
    warnings: &mut Vec<String>,
) {
    match parsed {
        Ok(value) => *slot = Some(value),
        Err(detail) => warnings.push(format!("ignored `{key}`: {detail}")),
    }
}

/// Reads the strict `update { … }` block. A field whose value is the wrong
/// kind fails the whole parse; an unknown field is dropped with a warning.
fn parse_update(
    node: &KdlNode,
    warnings: &mut Vec<String>,
) -> Result<PartialUpdateConfig, ConfigError> {
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
            other => warnings.push(format!(
                "ignored {}",
                unknown_key(
                    &format!("update.{other}"),
                    &[
                        "update.auto-check",
                        "update.check-interval-days",
                        "update.allow-prerelease",
                    ],
                )
            )),
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
            "gap" => set(&mut cfg.gap, value_u16(child), "pane", key, warnings),
            other => warnings.push(format!(
                "ignored {}",
                unknown_key(
                    &format!("pane.{other}"),
                    &["pane.min-cols", "pane.min-rows", "pane.gap"],
                )
            )),
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
            "scroll-on-input" => set(
                &mut cfg.scroll_on_input,
                value_bool(child),
                "scrollback",
                key,
                warnings,
            ),
            other => warnings.push(format!(
                "ignored {}",
                unknown_key(
                    &format!("scrollback.{other}"),
                    &[
                        "scrollback.max-lines",
                        "scrollback.max-bytes",
                        "scrollback.scroll-on-input",
                    ],
                )
            )),
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
            other => warnings.push(format!(
                "ignored {}",
                unknown_key(&format!("layout.{other}"), &["layout.new-pane-direction"],)
            )),
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
            other => warnings.push(format!(
                "ignored {}",
                unknown_key(
                    &format!("mouse.{other}"),
                    &["mouse.border-resize", "mouse.scroll-lines", "mouse.wheel",],
                )
            )),
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
            other => warnings.push(format!(
                "ignored {}",
                unknown_key(&format!("copy.{other}"), &["copy.trim-trailing-whitespace"],)
            )),
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
            // A blank or whitespace-only `term`/`colorterm` is dropped with a
            // warning.
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
            // it set, the inner is the shell. The file can only name a shell;
            // it cannot unset one. A blank value is dropped with a warning.
            "default-shell" => set(
                &mut cfg.default_shell,
                value_nonempty_string(child).map(Some),
                "terminal",
                key,
                warnings,
            ),
            other => warnings.push(format!(
                "ignored {}",
                unknown_key(
                    &format!("terminal.{other}"),
                    &[
                        "terminal.term",
                        "terminal.colorterm",
                        "terminal.default-shell",
                    ],
                )
            )),
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
            other => warnings.push(format!(
                "ignored {}",
                unknown_key(
                    &format!("logging.{other}"),
                    &["logging.enabled", "logging.level", "logging.format"],
                )
            )),
        }
    }
    cfg
}

/// Reads a scrollback cap. A negative value becomes `0` (no scrollback); a
/// value above `usize::MAX` becomes `usize::MAX`. `max-lines -5` yields `0`.
fn value_scrollback(node: &KdlNode) -> Result<usize, String> {
    Ok(value_integer(node)?.clamp(0, usize::MAX as i128) as usize)
}

/// Reads the node's single value as a split [`Direction`].
fn value_direction(node: &KdlNode) -> Result<Direction, String> {
    match value_string(node)? {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "up" => Ok(Direction::Up),
        "down" => Ok(Direction::Down),
        _ => Err(r#"expected "left", "right", "up", or "down""#.to_string()),
    }
}

/// Reads the node's single value as a [`LogLevel`], the lowest severity that
/// is written to the log file.
fn value_log_level(node: &KdlNode) -> Result<LogLevel, String> {
    match value_string(node)? {
        "info" => Ok(LogLevel::Info),
        "warning" => Ok(LogLevel::Warning),
        "error" => Ok(LogLevel::Error),
        _ => Err(r#"expected "info", "warning", or "error""#.to_string()),
    }
}

/// Reads the node's single value as a [`LogFormat`], the shape of each written
/// log line.
fn value_log_format(node: &KdlNode) -> Result<LogFormat, String> {
    match value_string(node)? {
        "pretty" => Ok(LogFormat::Pretty),
        "json" => Ok(LogFormat::Json),
        _ => Err(r#"expected "pretty" or "json""#.to_string()),
    }
}

/// Reads the node's single value as a [`WheelScroll`] behavior.
fn value_wheel(node: &KdlNode) -> Result<WheelScroll, String> {
    match value_string(node)? {
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

#[cfg(test)]
mod tests;
