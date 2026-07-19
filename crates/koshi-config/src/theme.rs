//! Parser for `theme.kdl`, the color-theme config file.
//!
//! Turns the file into a [`PartialThemeConfig`] override layer. Like the app
//! config it is **field-partial**: a color whose value is not a `#RRGGBB` hex
//! string is skipped — its default role color stands — and every other color
//! still applies, so one bad swatch never drops the whole theme. Each skipped
//! field is named in the returned warnings for the loader to log. Does no file
//! I/O: the caller reads the file and hands the text in.
//!
//! # Schema
//! Top-level nodes, no wrapping `theme` block (the file *is* the theme), the
//! same shape the keybinding file uses:
//! ```kdl
//! version 1          // optional; a version newer than this build is rejected
//! name "midnight"
//! colors {
//!     ramp-start "#581c87"
//!     accent "#a78bfa"
//!     border-focused "#00afd7"
//! }
//! ```

use std::path::Path;

use kdl::KdlNode;

use crate::error::{check_version, ConfigError};
use crate::layer::{PartialColorPalette, PartialThemeConfig};
use crate::parser::{parse_kdl, value_string, value_u32};
use crate::types::RgbColor;

/// Parses `theme.kdl` `source` into a [`PartialThemeConfig`] override layer and
/// the warning for every color that was skipped.
///
/// # Errors
/// Returns [`ConfigError::Parse`] when `source` is not valid KDL, and
/// [`ConfigError::Validation`] when its schema version is newer than this
/// build understands.
pub fn parse_theme(
    path: &Path,
    source: &str,
) -> Result<(PartialThemeConfig, Vec<String>), ConfigError> {
    let doc = parse_kdl(path, source)?;
    let mut theme = PartialThemeConfig::default();
    let mut warnings = Vec::new();
    for node in doc.nodes() {
        match node.name().value() {
            "version" => {
                let found = value_u32(node).map_err(|detail| validation("version", &detail))?;
                check_version(found)
                    .map_err(|diagnostic| validation("version", &diagnostic.to_string()))?;
            }
            "name" => set(&mut theme.name, value_string(node), "name", &mut warnings),
            "colors" => theme.colors = Some(parse_colors(node, &mut warnings)),
            // Unknown top-level nodes are ignored, matching the app config.
            _ => {}
        }
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
            other => {
                warnings.push(format!("ignored unknown `colors.{other}`"));
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
