//! Redaction helpers. A single place to scrub user data before it reaches logs,
//! debug dumps, snapshots, or IPC watchers.
//!
//! The safety property is enforced by the type system, not by discipline at call
//! sites: a [`RedactedValue::Hidden`] prints `***` in both `Display` and `Debug`,
//! so no caller can accidentally format a sensitive value. `RedactedValue` is
//! intentionally not `Serialize` — serializing a redacted map must go through a
//! `Display`-based dump, never a derived encoder that could emit the inner value.

use std::collections::BTreeMap;

/// What replaces a hidden value in any text output.
const REDACTED: &str = "***";

/// Case-insensitive key fragments that mark an environment variable as sensitive.
/// A key is redacted if it *contains* any of these.
const SENSITIVE_KEY_FRAGMENTS: [&str; 5] = ["TOKEN", "SECRET", "PASSWORD", "KEY", "AUTH"];

/// The in-session capability token. Any process in a pane inherits it and can act
/// as that pane, so it is always redacted. It already matches the
/// `TOKEN` fragment; this explicit guard is defense-in-depth against future edits
/// to [`SENSITIVE_KEY_FRAGMENTS`].
const ALWAYS_HIDDEN_KEY: &str = "TILE_CONTEXT_TOKEN";

/// An environment value after redaction. A `Hidden` value never reveals its
/// contents in any format; a `Visible` value carries a non-sensitive value through.
#[derive(Clone, PartialEq, Eq)]
pub enum RedactedValue {
    /// A non-sensitive value, passed through unchanged.
    Visible(String),
    /// A sensitive value, withheld. Always prints `***`.
    Hidden,
}

impl std::fmt::Display for RedactedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RedactedValue::Visible(value) => f.write_str(value),
            RedactedValue::Hidden => f.write_str(REDACTED),
        }
    }
}

impl std::fmt::Debug for RedactedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RedactedValue::Visible(value) => write!(f, "{value:?}"),
            RedactedValue::Hidden => f.write_str(REDACTED),
        }
    }
}

/// A known sensitive substring to scrub out of free-form text. Holds the literal
/// value (e.g. the actual context token) so [`redact_string`] removes it before a
/// command line or log line is recorded.
#[derive(Clone, PartialEq, Eq)]
pub struct Marker(String);

impl Marker {
    /// A marker matching every occurrence of `value` exactly.
    pub fn literal(value: impl Into<String>) -> Self {
        Marker(value.into())
    }
}

// A marker holds a real secret, so its `Debug` must never reveal the literal:
// tracing a struct that carries markers, or an assertion failure, prints `***`.
impl std::fmt::Debug for Marker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

/// True if `key` names a sensitive environment variable and must be redacted.
fn is_sensitive_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    if upper == ALWAYS_HIDDEN_KEY {
        return true;
    }
    SENSITIVE_KEY_FRAGMENTS
        .iter()
        .any(|fragment| upper.contains(fragment))
}

/// Redact an environment map by key. Keys naming a secret and `TILE_CONTEXT_TOKEN`
/// become [`RedactedValue::Hidden`] regardless of casing; all other values pass
/// through as [`RedactedValue::Visible`].
pub fn redact_env_map(env: &BTreeMap<String, String>) -> BTreeMap<String, RedactedValue> {
    env.iter()
        .map(|(key, value)| {
            let redacted = if is_sensitive_key(key) {
                RedactedValue::Hidden
            } else {
                RedactedValue::Visible(value.clone())
            };
            (key.clone(), redacted)
        })
        .collect()
}

/// Replace every occurrence of each marker's literal with `***`. Used to scrub
/// known secret values out of text before it is logged or dumped.
pub fn redact_string(input: &str, markers: &[Marker]) -> String {
    let mut secret_literals: Vec<&str> = markers
        .iter()
        .map(|marker| marker.0.as_str())
        .filter(|literal| !literal.is_empty())
        .collect();
    // Longest first: a short secret replaced before a longer overlapping one
    // would leave the longer one's tail visible ("abc" before "abcd" -> "***d").
    secret_literals.sort_by_key(|literal| std::cmp::Reverse(literal.len()));

    let mut out = input.to_string();
    for literal in secret_literals {
        out = out.replace(literal, REDACTED);
    }
    out
}

#[cfg(test)]
mod tests;
