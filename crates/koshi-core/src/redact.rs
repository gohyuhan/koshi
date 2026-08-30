//! Redaction helpers: scrub user data before it reaches logs, debug dumps,
//! snapshots, or IPC watchers.
//!
//! A [`RedactedValue::Hidden`] prints `***` in both `Display` and `Debug`.
//! `RedactedValue` is not `Serialize`: a redacted map is dumped through
//! `Display`.

use std::collections::BTreeMap;
use std::ops::Range;

/// What replaces a hidden value in any text output. Every type that withholds
/// a secret prints this.
pub const REDACTED: &str = "***";

/// Key fragments that mark an environment variable as sensitive. A key is
/// redacted if it *contains* any of these, compared ASCII-case-insensitively:
/// `KEYBOARD` and `Authorization` are redacted. `KOSHI_CONTEXT_TOKEN`, the
/// in-session capability token, is covered by the `TOKEN` fragment.
const SENSITIVE_KEY_FRAGMENTS: [&str; 5] = ["TOKEN", "SECRET", "PASSWORD", "KEY", "AUTH"];

/// An environment value after redaction. A `Hidden` value prints `***` in
/// `Display` and `Debug`; a `Visible` value carries a non-sensitive value
/// through and prints it as a plain `String` does.
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

/// A known sensitive substring that [`redact_string`] scrubs out of free-form
/// text. Holds the literal value, e.g. the actual context token. `Debug`
/// prints `***`, never the literal.
#[derive(Clone, PartialEq, Eq)]
pub struct Marker(String);

impl Marker {
    /// A marker matching every occurrence of `value` as a literal, not a
    /// pattern. An empty `value` matches nothing.
    pub fn literal(value: impl Into<String>) -> Self {
        Marker(value.into())
    }
}

impl std::fmt::Debug for Marker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(REDACTED)
    }
}

/// True if `key` contains any of [`SENSITIVE_KEY_FRAGMENTS`], compared
/// ASCII-case-insensitively.
fn is_sensitive_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    SENSITIVE_KEY_FRAGMENTS
        .iter()
        .any(|fragment| upper.contains(fragment))
}

/// Redact an environment map by key. A key containing `TOKEN`, `SECRET`,
/// `PASSWORD`, `KEY`, or `AUTH` in any ASCII case (`KOSHI_CONTEXT_TOKEN`,
/// `db_password`) maps to [`RedactedValue::Hidden`]; every other key maps to
/// [`RedactedValue::Visible`] holding its value. Keys are kept as they are.
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

/// Hide a spawned child's arguments: element 0, the program name, passes
/// through; every element after it becomes `***`, whatever it holds. An empty
/// `argv` yields an empty `Vec`.
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

/// Replace every occurrence of each marker's literal with `***`. Occurrences
/// that overlap or touch collapse into one `***`: `"abcd"` with markers `ab`
/// and `cd` results in `"***"`. Occurrences of one marker are found
/// left-to-right without overlap. An empty marker matches nothing.
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

    // 2. Merge overlapping and touching spans into one.
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
