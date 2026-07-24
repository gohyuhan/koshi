//! Parser for a theme file, one of the `themes/<name>.kdl` color themes.
//!
//! Turns the file into a [`PartialThemeConfig`] override layer. Like the app
//! config it is **field-partial**: a color whose value is not a `#RRGGBB` hex
//! string is skipped — its default role color stands — and every other color
//! still applies, so one bad swatch never drops the whole theme. Each skipped
//! field is named in the returned warnings for the loader to log. Does no file
//! I/O: the caller reads the file and hands the text in.
//!
//! The theme's name is its file name, so the file itself carries no name: the
//! loader fills [`PartialThemeConfig::name`] in from the stem of the path it
//! read.
//!
//! # Schema
//! Top-level nodes, no wrapping `theme` block (the file *is* the theme), the
//! same shape the keybinding file uses:
//! ```kdl
//! version 1
//! colors {
//!     ramp-start "#d0a5ff"
//!     accent "#f5c2ff"
//!     border-focused "#00afd7"
//! }
//! ```

use std::path::Path;

use kdl::KdlNode;

use crate::error::{check_version, ConfigError};
use crate::layer::{PartialColorPalette, PartialThemeConfig};
use crate::parser::{parse_kdl, unknown_key, value_string, value_u32};
use crate::types::RgbColor;

/// Parses a theme file's `source` into a [`PartialThemeConfig`] override layer
/// and the warning for every color that was skipped. The returned layer's
/// [`name`](PartialThemeConfig::name) is left unset: the theme is named by its
/// file, which the caller knows and this parser does not.
///
/// # Errors
/// Returns [`ConfigError::Parse`] when `source` is not valid KDL, and
/// [`ConfigError::Validation`] when its schema version is missing, duplicate,
/// zero, or newer than this build understands.
pub fn parse_theme(
    path: &Path,
    source: &str,
) -> Result<(PartialThemeConfig, Vec<String>), ConfigError> {
    let doc = parse_kdl(path, source)?;
    let mut theme = PartialThemeConfig::default();
    let mut warnings = Vec::new();
    let mut version_seen = false;
    let mut colors_seen = false;
    for node in doc.nodes() {
        match node.name().value() {
            "version" => {
                if version_seen {
                    return Err(validation(
                        "version",
                        "`version` is declared more than once",
                    ));
                }
                version_seen = true;
                if node.children().is_some() {
                    return Err(validation("version", "`version` takes no children"));
                }
                let found = value_u32(node).map_err(|detail| validation("version", &detail))?;
                check_version(found)
                    .map_err(|diagnostic| validation("version", &diagnostic.to_string()))?;
            }
            "colors" => {
                if colors_seen {
                    warnings.push("ignored duplicate `colors` section".to_string());
                } else {
                    colors_seen = true;
                    theme.colors = Some(parse_colors(node, &mut warnings));
                }
            }
            other => warnings.push(format!(
                "ignored {}",
                unknown_key(other, &["version", "colors"])
            )),
        }
    }
    if !version_seen {
        return Err(validation("version", "file must declare `version`"));
    }
    Ok((theme, warnings))
}

/// Reads the `colors { … }` block into per-role overrides.
fn parse_colors(node: &KdlNode, warnings: &mut Vec<String>) -> PartialColorPalette {
    let mut palette = PartialColorPalette::default();
    let Some(children) = node.children() else {
        return palette;
    };
    for child in children.nodes() {
        let key = child.name().value();
        let slot = match key {
            "ramp-start" => &mut palette.ramp_start,
            "ramp-end" => &mut palette.ramp_end,
            "on-ramp" => &mut palette.on_ramp,
            "on-ramp-dim" => &mut palette.on_ramp_dim,
            "accent" => &mut palette.accent,
            "on-accent" => &mut palette.on_accent,
            "border-focused" => &mut palette.border_focused,
            "border-unfocused" => &mut palette.border_unfocused,
            "border-hover" => &mut palette.border_hover,
            "stack-header-fg" => &mut palette.stack_header_fg,
            "stack-header-bg" => &mut palette.stack_header_bg,
            "letterbox" => &mut palette.letterbox,
            "bar-bg" => &mut palette.bar_bg,
            other => {
                warnings.push(format!(
                    "ignored {}",
                    unknown_key(
                        &format!("colors.{other}"),
                        &[
                            "colors.ramp-start",
                            "colors.ramp-end",
                            "colors.on-ramp",
                            "colors.on-ramp-dim",
                            "colors.accent",
                            "colors.on-accent",
                            "colors.border-focused",
                            "colors.border-unfocused",
                            "colors.border-hover",
                            "colors.stack-header-fg",
                            "colors.stack-header-bg",
                            "colors.letterbox",
                            "colors.bar-bg",
                        ],
                    )
                ));
                continue;
            }
        };
        let field = format!("colors.{key}");
        set(slot, value_color(child), &field, warnings);
    }
    palette
}

/// Stores a parsed field-partial value, or records a warning naming the field
/// and the reason it was skipped.
fn set<T>(
    slot: &mut Option<T>,
    parsed: Result<T, String>,
    field: &str,
    warnings: &mut Vec<String>,
) {
    match parsed {
        Ok(value) => *slot = Some(value),
        Err(detail) => warnings.push(format!("ignored `{field}`: {detail}")),
    }
}

/// Reads the node's single value as a `#RRGGBB` color.
fn value_color(node: &KdlNode) -> Result<RgbColor, String> {
    RgbColor::from_hex(&value_string(node)?).map_err(|err| err.to_string())
}

/// Builds a [`ConfigError::Validation`] for a bad top-level field.
fn validation(key: &str, detail: &str) -> ConfigError {
    ConfigError::Validation {
        key: key.to_string(),
        detail: detail.to_string(),
    }
}

#[cfg(test)]
mod tests;
