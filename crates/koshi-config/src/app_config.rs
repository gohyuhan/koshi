//! Parser for `koshi.kdl`, the app-settings config file.
//!
//! Turns the top-level sections of `koshi.kdl` into a [`PartialKoshiConfig`]
//! override layer that folds onto the built-in defaults. Today it understands
//! the `update` section; any other top-level node is left alone, so a file
//! carrying sections a later loader pass will own still parses here. Doing no
//! file I/O: the caller reads the file and hands the text in, as the
//! keybinding parser does.
//!
//! # Example
//! A `koshi.kdl` of
//! ```kdl
//! update {
//!     auto-check #false
//!     check-interval-days 30
//! }
//! ```
//! yields a layer whose `update` section sets `auto_check = false` and
//! `check_interval_days = 30`, leaving every other section untouched.

use std::path::Path;

use kdl::{KdlNode, KdlValue};

use crate::error::{check_version, ConfigError};
use crate::layer::{PartialKoshiConfig, PartialUpdateConfig};
use crate::parser::parse_kdl;

/// Parses `koshi.kdl` `source` into a [`PartialKoshiConfig`] override layer.
///
/// # Errors
/// Returns [`ConfigError::Parse`] when `source` is not valid KDL, and
/// [`ConfigError::Validation`] when a recognized field carries a value of the
/// wrong kind (e.g. `auto-check` set to a string).
pub fn parse_app_config(path: &Path, source: &str) -> Result<PartialKoshiConfig, ConfigError> {
    let doc = parse_kdl(path, source)?;
    let mut partial = PartialKoshiConfig::default();
    for node in doc.nodes() {
        match node.name().value() {
            // A file may declare its schema version; reject one newer than this
            // build understands, matching the other config files.
            "version" => {
                let found = read_u32(node, "version")?;
                check_version(found).map_err(|diagnostic| ConfigError::Validation {
                    key: "version".to_string(),
                    detail: diagnostic.to_string(),
                })?;
            }
            "update" => {
                if partial.update.is_some() {
                    return Err(validation("update", "duplicate `update` section"));
                }
                partial.update = Some(parse_update(node)?);
            }
            // Unknown top-level sections are ignored, not rejected: later loader
            // passes own them, and rejecting here would break a file that sets one.
            _ => {}
        }
    }
    Ok(partial)
}

/// Reads the children of an `update { … }` block into a partial section.
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
            _ => {}
        }
    }
    Ok(update)
}

/// Returns the node's single unnamed argument, or a validation error.
fn single_value<'a>(node: &'a KdlNode, key: &str) -> Result<&'a KdlValue, ConfigError> {
    match node.entries() {
        [entry] if entry.name().is_none() => Ok(entry.value()),
        _ => Err(validation(key, "expected exactly one value")),
    }
}

/// Reads the node's single value as a boolean.
fn read_bool(node: &KdlNode, key: &str) -> Result<bool, ConfigError> {
    single_value(node, key)?
        .as_bool()
        .ok_or_else(|| validation(key, "expected a boolean (#true or #false)"))
}

/// Reads the node's single value as a `u32`.
fn read_u32(node: &KdlNode, key: &str) -> Result<u32, ConfigError> {
    let n = single_value(node, key)?
        .as_integer()
        .ok_or_else(|| validation(key, "expected an integer"))?;
    u32::try_from(n).map_err(|_| validation(key, "must be between 0 and 4294967295"))
}

/// Builds a [`ConfigError::Validation`] for a bad `update` field value.
fn validation(key: &str, detail: &str) -> ConfigError {
    ConfigError::Validation {
        key: key.to_string(),
        detail: detail.to_string(),
    }
}

#[cfg(test)]
mod tests;
