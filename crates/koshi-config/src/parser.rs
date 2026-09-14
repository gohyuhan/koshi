//! KDL parsing entry point, and the field readers the file parsers share.
//!
//! [`parse_kdl`] wraps the `kdl` crate's document parser and attaches the
//! config file path to any syntax error as a [`ConfigParseDiagnostic`]. The
//! `parse_*_kdl_value` readers turn one field node into a typed field value,
//! `set_parsed_field` stores one field value or records the skip as a warning,
//! and [`format_unknown_key`] names the nearest allowed key for an unrecognized
//! one.

use std::path::Path;

use kdl::{KdlDiagnostic, KdlDocument, KdlError, KdlNode, KdlValue};
use miette::SourceSpan;

use crate::error::ConfigParseDiagnostic;

#[cfg(test)]
mod tests;

/// The deepest `{ … }` nesting [`parse_kdl`] reads.
///
/// The KDL parser recurses once per level and uses about 34 KiB of stack for
/// each, so 24 levels fit inside a 1 MiB thread stack. Koshi's own files nest
/// at most three levels deep.
pub(crate) const MAX_BLOCK_DEPTH: usize = 24;

/// The byte offset in `source_text` of the `{` that opens level
/// `MAX_BLOCK_DEPTH + 1`, or `None` when nothing nests that deep.
///
/// A `{` inside a line comment, a block comment, a quoted string or a raw
/// string opens no level. A `}` with no open level is ignored.
fn find_first_brace_past_depth_limit(source_text: &str) -> Option<usize> {
    let source_bytes = source_text.as_bytes();
    let mut depth: usize = 0;
    let mut block_comment_depth: usize = 0;
    let mut byte_index = 0;
    while byte_index < source_bytes.len() {
        let source_byte = source_bytes[byte_index];
        let next_source_byte = source_bytes.get(byte_index + 1).copied();
        if block_comment_depth > 0 {
            match (source_byte, next_source_byte) {
                (b'*', Some(b'/')) => {
                    block_comment_depth -= 1;
                    byte_index += 2;
                }
                (b'/', Some(b'*')) => {
                    block_comment_depth += 1;
                    byte_index += 2;
                }
                _ => byte_index += 1,
            }
            continue;
        }
        match source_byte {
            b'/' if next_source_byte == Some(b'/') => {
                byte_index += 2;
                while byte_index < source_bytes.len() && source_bytes[byte_index] != b'\n' {
                    byte_index += 1;
                }
            }
            b'/' if next_source_byte == Some(b'*') => {
                block_comment_depth = 1;
                byte_index += 2;
            }
            b'#' | b'"' => byte_index = find_string_end_offset(source_bytes, byte_index),
            b'{' => {
                depth += 1;
                if depth > MAX_BLOCK_DEPTH {
                    return Some(byte_index);
                }
                byte_index += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                byte_index += 1;
            }
            _ => byte_index += 1,
        }
    }
    None
}

/// The byte offset just past the string that starts at `start_byte_index`, where
/// `source_bytes[start_byte_index]` is `"` or `#`.
///
/// `#` runs that no `"` follows — `#true`, `#null` — are not strings, and the
/// answer is `start_byte_index + 1`. A string that never closes ends at
/// `source_bytes.len()`.
///
/// - `"a\"b" rest` from offset 0 → 6, the offset of the space
/// - `##"a"#b"## rest` from offset 0 → 10
/// - `#true` from offset 0 → 1
fn find_string_end_offset(source_bytes: &[u8], start_byte_index: usize) -> usize {
    let mut byte_index = start_byte_index;
    while source_bytes.get(byte_index) == Some(&b'#') {
        byte_index += 1;
    }
    let hash_character_count = byte_index - start_byte_index;
    if source_bytes.get(byte_index) != Some(&b'"') {
        return start_byte_index + 1;
    }
    byte_index += 1;
    if hash_character_count == 0 {
        while byte_index < source_bytes.len() {
            match source_bytes[byte_index] {
                b'\\' => byte_index += 2,
                b'"' => return byte_index + 1,
                _ => byte_index += 1,
            }
        }
        return source_bytes.len();
    }
    while byte_index < source_bytes.len() {
        if source_bytes[byte_index] == b'"'
            && source_bytes[byte_index + 1..]
                .iter()
                .take(hash_character_count)
                .all(|source_byte| *source_byte == b'#')
        {
            let string_end_offset = byte_index + 1 + hash_character_count;
            if string_end_offset <= source_bytes.len() {
                return string_end_offset;
            }
        }
        byte_index += 1;
    }
    source_bytes.len()
}

/// The refusal for source text that nests past [`MAX_BLOCK_DEPTH`], pointing at
/// the `{` at `depth_limit_byte_offset`.
fn build_nested_depth_diagnostic(
    config_path: &Path,
    config_source_text: &str,
    depth_limit_byte_offset: usize,
) -> ConfigParseDiagnostic {
    let config_source_text_arc = std::sync::Arc::new(config_source_text.to_string());
    ConfigParseDiagnostic::from_kdl_error(
        config_path,
        KdlError {
            input: config_source_text_arc.clone(),
            diagnostics: vec![KdlDiagnostic {
                input: config_source_text_arc,
                span: (depth_limit_byte_offset, 1).into(),
                message: Some(format!(
                    "blocks nest more than {MAX_BLOCK_DEPTH} levels deep"
                )),
                label: Some(format!("this block opens level {}", MAX_BLOCK_DEPTH + 1)),
                help: Some("flatten the nesting".to_string()),
                severity: miette::Severity::Error,
            }],
        },
    )
}

/// Parses `source_text` — the already-read contents of the config file at `path` —
/// into a [`KdlDocument`]. Does no file I/O: discovery and reading happen in
/// the caller.
///
/// # Errors
/// Returns a [`ConfigParseDiagnostic`] carrying `path` and the span-tagged
/// KDL error for pretty rendering when `source_text` is not valid KDL syntax, or
/// when a block nests more than 24 levels deep.
pub fn parse_kdl(
    config_path: &Path,
    config_source_text: &str,
) -> Result<KdlDocument, ConfigParseDiagnostic> {
    if let Some(depth_limit_byte_offset) = find_first_brace_past_depth_limit(config_source_text) {
        return Err(build_nested_depth_diagnostic(
            config_path,
            config_source_text,
            depth_limit_byte_offset,
        ));
    }
    config_source_text
        .parse::<KdlDocument>()
        .map_err(|parse_error| ConfigParseDiagnostic::from_kdl_error(config_path, parse_error))
}

// Field-value readers shared by the `koshi.kdl` and theme-file parsers. Each
// takes one field node (`key value`) and returns the typed field value, or a plain-words
// reason it could not be read.

/// The `{ … }` block of a section node, warning about a value written on the
/// section line, which no section reads.
///
/// `scrollback 5000` gives `None` and the warning ``ignored `scrollback`
/// value: a section takes a `{ … }` block``. `scrollback { max-lines 5000 }`
/// gives the block and no warning.
pub(crate) fn find_section_block<'a>(
    section_node: &'a KdlNode,
    parse_warnings: &mut Vec<String>,
) -> Option<&'a KdlDocument> {
    if !section_node.entries().is_empty() {
        parse_warnings.push(format!(
            "ignored `{}` value: a section takes a `{{ … }}` block",
            section_node.name().value()
        ));
    }
    section_node.children()
}

/// The node's single unnamed argument.
///
/// Returns `takes no children` when the node carries a `{ … }` child block:
/// `theme "midnight" { foo }` is an error. Returns `expected exactly one
/// value` when the node holds anything other than one unnamed argument.
pub(crate) fn parse_single_kdl_value(field_node: &KdlNode) -> Result<&KdlValue, String> {
    if field_node.children().is_some() {
        return Err("takes no children".to_string());
    }
    match field_node.entries() {
        [argument_entry] if argument_entry.name().is_none() => Ok(argument_entry.value()),
        _ => Err("expected exactly one value".to_string()),
    }
}

/// Reads the node's single value as a boolean, or
/// `expected a boolean (#true or #false)`.
pub(crate) fn parse_boolean_kdl_value(field_node: &KdlNode) -> Result<bool, String> {
    parse_single_kdl_value(field_node)?
        .as_bool()
        .ok_or_else(|| "expected a boolean (#true or #false)".to_string())
}

/// Reads the node's single value as a string, borrowed from the node, or
/// `expected a string`.
pub(crate) fn parse_string_kdl_value(field_node: &KdlNode) -> Result<&str, String> {
    parse_single_kdl_value(field_node)?
        .as_string()
        .ok_or_else(|| "expected a string".to_string())
}

/// Reads the node's single value as a string, **trimmed** of surrounding
/// whitespace. A value that is empty or whitespace-only is rejected with
/// `must not be empty`.
///
/// `term " xterm-256color "` yields `xterm-256color`; `theme " midnight "`
/// yields `midnight`; `term "   "` and `term ""` are both errors.
pub(crate) fn parse_nonempty_string_kdl_value(field_node: &KdlNode) -> Result<String, String> {
    let trimmed_text = parse_string_kdl_value(field_node)?.trim();
    if trimmed_text.is_empty() {
        Err("must not be empty".to_string())
    } else {
        Ok(trimmed_text.to_string())
    }
}

/// Reads the node's single value as an integer, or `expected an integer`.
pub(crate) fn parse_integer_kdl_value(field_node: &KdlNode) -> Result<i128, String> {
    parse_single_kdl_value(field_node)?
        .as_integer()
        .ok_or_else(|| "expected an integer".to_string())
}

/// Reads the node's single value as a `u16`. A value outside `0..=65535`
/// gives `must be between 0 and 65535`.
pub(crate) fn parse_u16_kdl_value(field_node: &KdlNode) -> Result<u16, String> {
    u16::try_from(parse_integer_kdl_value(field_node)?)
        .map_err(|_| "must be between 0 and 65535".to_string())
}

/// Reads the node's single value as a `u32`. A value outside `0..=4294967295`
/// gives `must be between 0 and 4294967295`.
pub(crate) fn parse_u32_kdl_value(field_node: &KdlNode) -> Result<u32, String> {
    u32::try_from(parse_integer_kdl_value(field_node)?)
        .map_err(|_| "must be between 0 and 4294967295".to_string())
}

/// Reads the schema number a `version` node declares. Every config file that
/// carries a `version` node reads it through this function, so one mistake
/// reads the same way in `koshi.kdl`, a theme, `keybinding.kdl`, a profile,
/// and migration.
///
/// `version 3` gives `Ok(3)`.
///
/// # Errors
/// Each error carries the span a caret points at and the reason:
///
/// - the whole node and ``` `version` takes no children ``` when the node
///   carries a `{ … }` block;
/// - the whole node and ``` `version` takes exactly one integer argument ```
///   when the node's arguments are not exactly one unnamed value;
/// - the argument and ``` `version` must be an integer from 1 to 4294967295 ```
///   when that value is not an integer or falls outside `0..=4294967295`.
///
/// A declared `0` is returned as `Ok(0)`;
/// [`validate_config_schema_version`](crate::error::validate_config_schema_version) rejects it.
pub(crate) fn parse_version_argument(
    version_node: &KdlNode,
) -> Result<u32, (SourceSpan, &'static str)> {
    if version_node.children().is_some() {
        return Err((version_node.span(), "`version` takes no children"));
    }
    let [version_argument] = version_node.entries() else {
        return Err((
            version_node.span(),
            "`version` takes exactly one integer argument",
        ));
    };
    if version_argument.name().is_some() {
        return Err((
            version_node.span(),
            "`version` takes exactly one integer argument",
        ));
    }
    let schema_version_error_message = "`version` must be an integer from 1 to 4294967295";
    let schema_version = version_argument
        .value()
        .as_integer()
        .ok_or((version_argument.span(), schema_version_error_message))
        .and_then(|schema_version_value| {
            u32::try_from(schema_version_value)
                .map_err(|_| (version_argument.span(), schema_version_error_message))
        })?;
    Ok(schema_version)
}

/// Stores a parsed field value in `parsed_field_value`. On `Err`, leaves the
/// field value untouched and pushes one warning naming the field and reason.
///
/// `section_name` is the enclosing block (`pane`), `key_name` the field node's
/// name (`min-cols`). A `field_parse_result` of `Err("expected an integer")` pushes
/// ``ignored `pane.min-cols`: expected an integer``.
pub(crate) fn set_parsed_field<FieldValue>(
    field_value_slot: &mut Option<FieldValue>,
    field_parse_result: Result<FieldValue, String>,
    section_name: &str,
    field_name: &str,
    parse_warnings: &mut Vec<String>,
) {
    match field_parse_result {
        Ok(parsed_field_value) => *field_value_slot = Some(parsed_field_value),
        Err(parse_error_message) => parse_warnings.push(format!(
            "ignored `{section_name}.{field_name}`: {parse_error_message}"
        )),
    }
}

/// Names the nearest allowed key for an unknown config key, measured by
/// Levenshtein edit distance in characters. A tie goes to the earliest key in
/// `allowed_key_names`.
///
/// `format_unknown_key("pane.min-col", &["pane.min-cols", "pane.min-rows"])` gives
/// ``unknown key `pane.min-col`; did you mean `pane.min-cols`?``.
///
/// # Panics
/// Panics when `allowed_key_names` is empty.
#[must_use]
pub fn format_unknown_key(unknown_key_name: &str, allowed_key_names: &[&str]) -> String {
    let nearest_key_name = allowed_key_names
        .iter()
        .min_by_key(|candidate_key_name| {
            compute_edit_distance(unknown_key_name, candidate_key_name)
        })
        .expect("every config key set is non-empty");
    format!("unknown key `{unknown_key_name}`; did you mean `{nearest_key_name}`?")
}

/// The Levenshtein edit distance between `left` and `right`, counted in
/// characters. `"colors.acent"` against `"colors.accent"` is `1`.
fn compute_edit_distance(left_text: &str, right_text: &str) -> usize {
    let right_character_count = right_text.chars().count();
    let mut previous_row: Vec<usize> = (0..=right_character_count).collect();
    let mut current_row = vec![0; previous_row.len()];
    for (left_character_index, left_character) in left_text.chars().enumerate() {
        current_row[0] = left_character_index + 1;
        for (right_character_index, right_character) in right_text.chars().enumerate() {
            current_row[right_character_index + 1] = if left_character == right_character {
                previous_row[right_character_index]
            } else {
                1 + previous_row[right_character_index]
                    .min(current_row[right_character_index])
                    .min(previous_row[right_character_index + 1])
            };
        }
        std::mem::swap(&mut previous_row, &mut current_row);
    }
    previous_row[right_character_count]
}
