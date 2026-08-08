//! Redaction helpers. A single place to scrub user data before it reaches logs,
//! debug dumps, snapshots, or IPC watchers.
//!
//! A [`RedactedValue::Hidden`] prints `***` in both `Display` and `Debug`, so no
//! formatting call can reveal a sensitive value. `RedactedValue` is not
//! `Serialize`: a redacted map is dumped through `Display`.

use std::collections::BTreeMap;
use std::ops::Range;

/// What replaces a hidden value in any text output. Every type that withholds
/// a secret prints this, so redacted output looks the same wherever it appears.
pub const REDACTED: &str = "***";

/// Case-insensitive key fragments that mark an environment variable as sensitive.
/// A key is redacted if it *contains* any of these.
const SENSITIVE_KEY_FRAGMENTS: [&str; 5] = ["TOKEN", "SECRET", "PASSWORD", "KEY", "AUTH"];

/// The in-session capability token. Any process in a pane inherits it and can
/// act as that pane, so it is always redacted — by this name and by the `TOKEN`
/// fragment in [`SENSITIVE_KEY_FRAGMENTS`].
const ALWAYS_HIDDEN_KEY: &str = "KOSHI_CONTEXT_TOKEN";

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

/// Redact an environment map by key. Keys naming a secret and `KOSHI_CONTEXT_TOKEN`
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

/// Hide a spawned child's arguments: element 0 passes through, every element
/// after it becomes `***`. Pass an argv whose element 0 is the program name —
/// every element after it can hold a secret.
///
/// `["mysql", "-pHUNTER2"]` results in `["mysql", "***"]`.
pub fn redact_argv(argv: &[String]) -> Vec<String> {
    argv.iter()
        .enumerate()
        .map(|(index, arg)| {
            if index == 0 {
                arg.clone()
            } else {
                REDACTED.to_string()
            }
        })
        .collect()
}

/// Replace every occurrence of each marker's literal with `***`. Used to scrub
/// known secret values out of text before it is logged or dumped.
pub fn redact_string(input: &str, markers: &[Marker]) -> String {
    // 1. Find every byte range of `input` a secret covers.
    let mut spans: Vec<Range<usize>> = Vec::new();
    for marker in markers {
        let secret = marker.0.as_str();
        if secret.is_empty() {
            continue;
        }
        for (start, found) in input.match_indices(secret) {
            spans.push(start..start + found.len());
        }
    }

    // 2. Merge overlapping spans, so an overlap is redacted once as a whole.
    spans.sort_by_key(|span| span.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for span in spans {
        if let Some(last) = merged.last_mut() {
            if span.start <= last.end {
                last.end = last.end.max(span.end);
                continue;
            }
        }
        merged.push(span);
    }

    // 3. Rebuild: copy the text between spans, replace each span with `***`.
    let mut out = String::new();
    let mut cursor = 0;
    for span in merged {
        out.push_str(&input[cursor..span.start]);
        out.push_str(REDACTED);
        cursor = span.end;
    }
    out.push_str(&input[cursor..]);
    out
}

#[cfg(test)]
mod tests;
