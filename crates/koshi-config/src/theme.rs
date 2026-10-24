//! Parser for a theme file, one of the `themes/<name>.kdl` color themes.
//!
//! Turns the file into a [`PartialThemeConfig`] override layer. Like the app
//! config it is **field-partial**: a color whose value is not six hex digits,
//! with or without a leading `#`, is skipped — its default role color stands —
//! and every other color still applies. Each skipped field is named in the
//! returned warnings for the loader to log. Does no file I/O: the caller reads
//! the file and hands the text in.
//!
//! The theme's name is its file name, so the file itself carries no name: the
//! loader fills [`PartialThemeConfig::theme_name`] in from the stem of the path it
//! read.
//!
//! # Schema
//! Top-level nodes, no wrapping `theme` block (the file *is* the theme), the
//! same shape the keybinding file uses:
//! ```kdl
//! version 2
//! colors {
//!     ramp-start "#d0a5ff"
//!     accent "#f5c2ff"
//!     border-focused "#00afd7"
//! }
//! ```

use std::path::Path;

use kdl::{KdlDocument, KdlNode};

use crate::error::{build_validation_error, ConfigError};
use crate::layer::{PartialColorPalette, PartialThemeConfig};
use crate::parser::{
    find_section_block, format_unknown_key, parse_kdl, parse_string_kdl_value, set_parsed_field,
    validate_version_node, ParseWarnings,
};
use crate::types::RgbColor;

/// The node names allowed inside `colors`, each already carrying the
/// `colors.` prefix an unknown key is matched against for the `did you mean`
/// hint. Same order as the arms of [`parse_color_section`].
const COLOR_KEYS: &[&str] = &[
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
];

/// Parses a theme file's `theme_source_text` into a [`PartialThemeConfig`]
/// override layer and one warning per skipped color, unknown key, value
/// written on the `colors` line, and repeated `colors` block, in file order.
/// Each warning starts with the line and column of its node, such as
/// ``line 3, column 5: ignored `colors.accent`: color must be 6 hex digits
/// (#RRGGBB), got 3``.
/// The returned layer leaves [`theme_name`](PartialThemeConfig::theme_name)
/// unset, and the caller sets it from the file name.
///
/// # Errors
/// Returns [`ConfigError::Parse`] when `theme_source_text` is not valid KDL.
///
/// Returns [`ConfigError::Validation`] with key `version` when `version` is
/// missing, declared twice, carries a `{ … }` block, is not a single integer
/// from `0` to `4294967295`, is `0`, or is newer than this build supports.
pub fn parse_theme(
    theme_path: &Path,
    theme_source_text: &str,
) -> Result<(PartialThemeConfig, Vec<String>), ConfigError> {
    let theme_document = parse_kdl(theme_path, theme_source_text)?;
    parse_theme_from_document(&theme_document, theme_source_text)
}

/// [`parse_theme`] on `theme_document`, the KDL document already parsed from
/// `theme_source_text`.
///
/// # Errors
/// Returns the [`ConfigError::Validation`] errors of [`parse_theme`].
pub(crate) fn parse_theme_from_document(
    theme_document: &KdlDocument,
    theme_source_text: &str,
) -> Result<(PartialThemeConfig, Vec<String>), ConfigError> {
    let mut partial_theme_config = PartialThemeConfig::default();
    let mut parse_warnings = ParseWarnings::default();
    let mut has_seen_version = false;
    let mut has_seen_colors = false;
    for config_node in theme_document.nodes() {
        match config_node.name().value() {
            "version" => {
                if has_seen_version {
                    return Err(build_validation_error(
                        "version",
                        "`version` is declared more than once",
                    ));
                }
                has_seen_version = true;
                validate_version_node(config_node)?;
            }
            "colors" => {
                if has_seen_colors {
                    parse_warnings
                        .record_warning(config_node.span(), "ignored duplicate `colors` section");
                } else {
                    has_seen_colors = true;
                    partial_theme_config.colors =
                        Some(parse_color_section(config_node, &mut parse_warnings));
                }
            }
            unknown_section_name => parse_warnings.record_warning(
                config_node.span(),
                format!(
                    "ignored {}",
                    format_unknown_key(unknown_section_name, &["version", "colors"])
                ),
            ),
        }
    }
    if !has_seen_version {
        return Err(build_validation_error(
            "version",
            "file must declare `version`",
        ));
    }
    Ok((
        partial_theme_config,
        parse_warnings.format_in_file_order(theme_source_text),
    ))
}

/// Reads the `colors { … }` block into per-role overrides. A role whose value
/// is unreadable, and a name outside [`COLOR_KEYS`], are left unset and
/// recorded in `parse_warnings`. A `colors` node with no `{ … }` block sets no role, and a
/// value written on the `colors` line itself is warned about and ignored.
fn parse_color_section(
    config_node: &KdlNode,
    parse_warnings: &mut ParseWarnings,
) -> PartialColorPalette {
    let mut color_palette = PartialColorPalette::default();
    let Some(color_section_children) = find_section_block(config_node, parse_warnings) else {
        return color_palette;
    };
    for color_node in color_section_children.nodes() {
        let color_field_name = color_node.name().value();
        let color_slot = match color_field_name {
            "ramp-start" => &mut color_palette.ramp_start,
            "ramp-end" => &mut color_palette.ramp_end,
            "on-ramp" => &mut color_palette.on_ramp,
            "on-ramp-dim" => &mut color_palette.on_ramp_dim,
            "accent" => &mut color_palette.accent,
            "on-accent" => &mut color_palette.on_accent,
            "border-focused" => &mut color_palette.border_focused,
            "border-unfocused" => &mut color_palette.border_unfocused,
            "border-hover" => &mut color_palette.border_hover,
            "stack-header-fg" => &mut color_palette.stack_header_fg,
            "stack-header-bg" => &mut color_palette.stack_header_bg,
            "letterbox" => &mut color_palette.letterbox,
            "bar-bg" => &mut color_palette.bar_bg,
            unknown_color_name => {
                parse_warnings.record_warning(
                    color_node.span(),
                    format!(
                        "ignored {}",
                        format_unknown_key(&format!("colors.{unknown_color_name}"), COLOR_KEYS)
                    ),
                );
                continue;
            }
        };
        set_parsed_field(
            color_slot,
            parse_rgb_color(color_node),
            "colors",
            color_node,
            parse_warnings,
        );
    }
    color_palette
}

/// Reads the node's single value as a color of six hex digits, with or
/// without a leading `#`: `"#a78bfa"` and `"a78bfa"` both give
/// `RgbColor::from_channels(0xa7, 0x8b, 0xfa)`.
fn parse_rgb_color(color_node: &KdlNode) -> Result<RgbColor, String> {
    RgbColor::from_hex(parse_string_kdl_value(color_node)?)
        .map_err(|color_parse_error| color_parse_error.to_string())
}

#[cfg(test)]
mod tests;
