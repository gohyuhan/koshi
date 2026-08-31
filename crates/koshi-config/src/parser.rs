//! KDL parsing entry point, and the field readers the file parsers share.
//!
//! [`parse_kdl`] wraps the `kdl` crate's document parser and attaches the
//! config file path to any syntax error as a [`ConfigParseDiagnostic`]. The
//! `value_*` readers turn one field node into a typed value, `set` stores one
//! such value or records the skip as a warning, and [`unknown_key`] names the
//! nearest allowed key for an unrecognized one.

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

/// The byte offset in `source` of the `{` that opens level
/// `MAX_BLOCK_DEPTH + 1`, or `None` when nothing nests that deep.
///
/// A `{` inside a line comment, a block comment, a quoted string or a raw
/// string opens no level. A `}` with no open level is ignored.
fn first_brace_past_the_depth_limit(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth: usize = 0;
    let mut block_comments: usize = 0;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if block_comments > 0 {
            match (byte, next) {
                (b'*', Some(b'/')) => {
                    block_comments -= 1;
                    index += 2;
                }
                (b'/', Some(b'*')) => {
                    block_comments += 1;
                    index += 2;
                }
                _ => index += 1,
            }
            continue;
        }
        match byte {
            b'/' if next == Some(b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if next == Some(b'*') => {
                block_comments = 1;
                index += 2;
            }
            b'#' | b'"' => index = past_string(bytes, index),
            b'{' => {
                depth += 1;
                if depth > MAX_BLOCK_DEPTH {
                    return Some(index);
                }
                index += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

/// The byte offset just past the string that starts at `start`, where
/// `bytes[start]` is `"` or `#`.
///
/// `#` runs that no `"` follows — `#true`, `#null` — are not strings, and the
/// answer is `start + 1`. A string that never closes ends at `bytes.len()`.
///
/// - `"a\"b" rest` from offset 0 → 6, the offset of the space
/// - `##"a"#b"## rest` from offset 0 → 10
/// - `#true` from offset 0 → 1
fn past_string(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    while bytes.get(index) == Some(&b'#') {
        index += 1;
    }
    let hashes = index - start;
    if bytes.get(index) != Some(&b'"') {
        return start + 1;
    }
    index += 1;
    if hashes == 0 {
        while index < bytes.len() {
            match bytes[index] {
                b'\\' => index += 2,
                b'"' => return index + 1,
                _ => index += 1,
            }
        }
        return bytes.len();
    }
    while index < bytes.len() {
        if bytes[index] == b'"' && bytes[index + 1..].iter().take(hashes).all(|b| *b == b'#') {
            let closed = index + 1 + hashes;
            if closed <= bytes.len() {
                return closed;
            }
        }
        index += 1;
    }
    bytes.len()
}

/// The refusal for a `source` that nests past [`MAX_BLOCK_DEPTH`], pointing at
/// the `{` at byte `offset`.
fn nested_too_deep(path: &Path, source: &str, offset: usize) -> ConfigParseDiagnostic {
    let input = std::sync::Arc::new(source.to_string());
    ConfigParseDiagnostic::new(
        path,
        KdlError {
            input: input.clone(),
            diagnostics: vec![KdlDiagnostic {
                input,
                span: (offset, 1).into(),
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

/// Parses `source` — the already-read contents of the config file at `path` —
/// into a [`KdlDocument`]. Does no file I/O: discovery and reading happen in
/// the caller.
///
/// # Errors
/// Returns a [`ConfigParseDiagnostic`] carrying `path` and the span-tagged
/// KDL error for pretty rendering when `source` is not valid KDL syntax, or
/// when a block nests more than 24 levels deep.
pub fn parse_kdl(path: &Path, source: &str) -> Result<KdlDocument, ConfigParseDiagnostic> {
    if let Some(offset) = first_brace_past_the_depth_limit(source) {
        return Err(nested_too_deep(path, source, offset));
    }
    source
        .parse::<KdlDocument>()
        .map_err(|err| ConfigParseDiagnostic::new(path, err))
}

// Field-value readers shared by the `koshi.kdl` and theme-file parsers. Each
// takes one field node (`key value`) and returns the value, or a plain-words
// reason it could not be read.

/// The `{ … }` block of a section node, warning about a value written on the
/// section line, which no section reads.
///
/// `scrollback 5000` gives `None` and the warning ``ignored `scrollback`
/// value: a section takes a `{ … }` block``. `scrollback { max-lines 5000 }`
/// gives the block and no warning.
pub(crate) fn section_block<'a>(
    node: &'a KdlNode,
    warnings: &mut Vec<String>,
) -> Option<&'a KdlDocument> {
    if !node.entries().is_empty() {
        warnings.push(format!(
            "ignored `{}` value: a section takes a `{{ … }}` block",
            node.name().value()
        ));
    }
    node.children()
}

/// The node's single unnamed argument.
///
/// Returns `takes no children` when the node carries a `{ … }` child block:
/// `theme "midnight" { foo }` is an error. Returns `expected exactly one
/// value` when the node holds anything other than one unnamed argument.
pub(crate) fn single_value(node: &KdlNode) -> Result<&KdlValue, String> {
    if node.children().is_some() {
        return Err("takes no children".to_string());
    }
    match node.entries() {
        [entry] if entry.name().is_none() => Ok(entry.value()),
        _ => Err("expected exactly one value".to_string()),
    }
}

/// Reads the node's single value as a boolean, or
/// `expected a boolean (#true or #false)`.
pub(crate) fn value_bool(node: &KdlNode) -> Result<bool, String> {
    single_value(node)?
        .as_bool()
        .ok_or_else(|| "expected a boolean (#true or #false)".to_string())
}

/// Reads the node's single value as a string, borrowed from the node, or
/// `expected a string`.
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

/// Reads the node's single value as an integer, or `expected an integer`.
pub(crate) fn value_integer(node: &KdlNode) -> Result<i128, String> {
    single_value(node)?
        .as_integer()
        .ok_or_else(|| "expected an integer".to_string())
}

/// Reads the node's single value as a `u16`. A value outside `0..=65535`
/// gives `must be between 0 and 65535`.
pub(crate) fn value_u16(node: &KdlNode) -> Result<u16, String> {
    u16::try_from(value_integer(node)?).map_err(|_| "must be between 0 and 65535".to_string())
}

/// Reads the node's single value as a `u32`. A value outside `0..=4294967295`
/// gives `must be between 0 and 4294967295`.
pub(crate) fn value_u32(node: &KdlNode) -> Result<u32, String> {
    u32::try_from(value_integer(node)?).map_err(|_| "must be between 0 and 4294967295".to_string())
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
/// [`check_version`](crate::error::check_version) rejects it.
pub(crate) fn version_arg(node: &KdlNode) -> Result<u32, (SourceSpan, &'static str)> {
    if node.children().is_some() {
        return Err((node.span(), "`version` takes no children"));
    }
    let [entry] = node.entries() else {
        return Err((node.span(), "`version` takes exactly one integer argument"));
    };
    if entry.name().is_some() {
        return Err((node.span(), "`version` takes exactly one integer argument"));
    }
    let range = "`version` must be an integer from 1 to 4294967295";
    let value = entry
        .value()
        .as_integer()
        .ok_or((entry.span(), range))
        .and_then(|value| u32::try_from(value).map_err(|_| (entry.span(), range)))?;
    Ok(value)
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

/// Names the nearest allowed key for an unknown config key, measured by
/// Levenshtein edit distance in characters. A tie goes to the earliest entry
/// in `allowed`.
///
/// `unknown_key("pane.min-col", &["pane.min-cols", "pane.min-rows"])` gives
/// ``unknown key `pane.min-col`; did you mean `pane.min-cols`?``.
///
/// # Panics
/// Panics when `allowed` is empty.
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
