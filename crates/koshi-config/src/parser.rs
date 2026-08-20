//! KDL parsing entry point, and the field readers the file parsers share.
//!
//! [`parse_kdl`] wraps the `kdl` crate's document parser and attaches the
//! config file path to any syntax error as a [`ConfigParseDiagnostic`]. The
//! `value_*` readers turn one field node into a typed value, `set` stores one
//! such value or records the skip as a warning, and [`unknown_key`] names the
//! nearest allowed key for an unrecognized one.

use std::path::Path;

use kdl::{KdlDocument, KdlNode, KdlValue};

use crate::error::ConfigParseDiagnostic;

#[cfg(test)]
mod tests;

/// Parses `source` — the already-read contents of the config file at `path` —
/// into a [`KdlDocument`]. Does no file I/O: discovery and reading happen in
/// the caller.
///
/// # Errors
/// Returns a [`ConfigParseDiagnostic`] carrying `path` and the span-tagged
/// KDL error for pretty rendering when `source` is not valid KDL syntax.
pub fn parse_kdl(path: &Path, source: &str) -> Result<KdlDocument, ConfigParseDiagnostic> {
    source
        .parse::<KdlDocument>()
        .map_err(|err| ConfigParseDiagnostic::new(path, err))
}

// Field-value readers shared by the `koshi.kdl` and theme-file parsers. Each
// takes one field node (`key value`) and returns the value, or a plain-words
// reason it could not be read.

/// The node's single unnamed argument, or a plain-words reason it is missing.
/// A node carrying a `{ … }` child block is refused: every scalar field takes
/// a value alone, so `theme "midnight" { foo }` is an error, not a silently
/// dropped block.
pub(crate) fn single_value(node: &KdlNode) -> Result<&KdlValue, String> {
    if node.children().is_some() {
        return Err("takes no children".to_string());
    }
    match node.entries() {
        [entry] if entry.name().is_none() => Ok(entry.value()),
        _ => Err("expected exactly one value".to_string()),
    }
}

/// Reads the node's single value as a boolean.
pub(crate) fn value_bool(node: &KdlNode) -> Result<bool, String> {
    single_value(node)?
        .as_bool()
        .ok_or_else(|| "expected a boolean (#true or #false)".to_string())
}

/// Reads the node's single value as a string, borrowed from the node.
pub(crate) fn value_string(node: &KdlNode) -> Result<&str, String> {
    single_value(node)?
        .as_string()
        .ok_or_else(|| "expected a string".to_string())
}

/// Reads the node's single value as a string, **trimmed** of surrounding
/// whitespace. A value that is empty or whitespace-only is rejected with
/// `must not be empty`.
///
/// `term " xterm-256color "` yields `xterm-256color`; `theme " midnight "`
/// yields `midnight`; `term "   "` and `term ""` are both errors.
pub(crate) fn value_nonempty_string(node: &KdlNode) -> Result<String, String> {
    let trimmed = value_string(node)?.trim();
    if trimmed.is_empty() {
        Err("must not be empty".to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

/// Reads the node's single value as an integer.
pub(crate) fn value_integer(node: &KdlNode) -> Result<i128, String> {
    single_value(node)?
        .as_integer()
        .ok_or_else(|| "expected an integer".to_string())
}

/// Reads the node's single value as a `u16`.
pub(crate) fn value_u16(node: &KdlNode) -> Result<u16, String> {
    u16::try_from(value_integer(node)?).map_err(|_| "must be between 0 and 65535".to_string())
}

/// Reads the node's single value as a `u32`.
pub(crate) fn value_u32(node: &KdlNode) -> Result<u32, String> {
    u32::try_from(value_integer(node)?).map_err(|_| "must be between 0 and 4294967295".to_string())
}

/// Stores a parsed field-partial value in `slot`. On `Err`, leaves `slot`
/// untouched and pushes one warning naming the field and the reason.
///
/// `section` is the enclosing block (`pane`), `key` the field node's name
/// (`min-cols`). A `parsed` of `Err("expected an integer")` pushes
/// ``ignored `pane.min-cols`: expected an integer``.
pub(crate) fn set<T>(
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

/// Names the nearest allowed key for an unknown config key.
#[must_use]
pub fn unknown_key(key: &str, allowed: &[&str]) -> String {
    let nearest = allowed
        .iter()
        .min_by_key(|candidate| edit_distance(key, candidate))
        .expect("every config key set is non-empty");
    format!("unknown key `{key}`; did you mean `{nearest}`?")
}

/// The Levenshtein edit distance between `left` and `right`, counted in
/// characters. `"colors.acent"` against `"colors.accent"` is `1`.
fn edit_distance(left: &str, right: &str) -> usize {
    let right_len = right.chars().count();
    let mut previous: Vec<usize> = (0..=right_len).collect();
    let mut current = vec![0; previous.len()];
    for (left_index, left_char) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right.chars().enumerate() {
            current[right_index + 1] = if left_char == right_char {
                previous[right_index]
            } else {
                1 + previous[right_index]
                    .min(current[right_index])
                    .min(previous[right_index + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right_len]
}
