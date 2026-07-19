//! KDL parsing entry point. Wraps the `kdl` crate's document parser and attaches
//! the config file path to any syntax error as a [`ConfigParseDiagnostic`].

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

// Field-value readers shared by the `koshi.kdl` and `theme.kdl` parsers. Each
// takes one field node (`key value`) and returns the value or a plain-words
// reason it could not be read, so a field-partial parser can turn that reason
// into a warning and skip the field.

/// The node's single unnamed argument, or a plain-words reason it is missing.
pub(crate) fn single_value(node: &KdlNode) -> Result<&KdlValue, String> {
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

/// Reads the node's single value as a string.
pub(crate) fn value_string(node: &KdlNode) -> Result<String, String> {
    single_value(node)?
        .as_string()
        .map(str::to_string)
        .ok_or_else(|| "expected a string".to_string())
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

/// Reads the node's single value as a `usize`.
pub(crate) fn value_usize(node: &KdlNode) -> Result<usize, String> {
    usize::try_from(value_integer(node)?)
        .map_err(|_| "must be a non-negative whole number".to_string())
}
