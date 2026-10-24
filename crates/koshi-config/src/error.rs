//! Config domain errors.
//!
//! [`ConfigError`] is the plain error enum other code matches on.
//! [`ConfigParseDiagnostic`] is a richer parse error that keeps the original
//! KDL source text and the byte span of the failure; a rendered report points
//! a caret at the bad line. `From` converts it into a plain
//! [`ConfigError::Parse`]. [`ConfigVersionDiagnostic`] reports a declared schema version
//! that is zero or newer than this build supports.
//! [`ColorParseError`] reports a theme color value that is not valid
//! `#RRGGBB` hex.

use std::fmt::Display;
use std::path::Path;

use kdl::KdlError;
use miette::{Diagnostic, SourceCode};
use thiserror::Error;

use crate::types::SCHEMA_VERSION;

/// A failure in config discovery, parsing, or validation. Config problems are
/// recoverable: Koshi falls back to defaults and surfaces the issue to the user.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The config file could not be parsed.
    #[error("config parse error in {config_path}: {parse_error_detail}")]
    Parse {
        /// The path of the config file that failed to parse.
        config_path: String,
        /// The first KDL sub-diagnostic as `line <L>, column <C>: <message>`,
        /// or the KDL error's own message when it has none.
        parse_error_detail: String,
    },
    /// The config parsed but failed schema validation.
    #[error("invalid config key `{config_key}`: {validation_detail}")]
    Validation {
        /// The config key that failed validation.
        config_key: String,
        /// The plain-word reason the value is invalid.
        validation_detail: String,
    },
}

/// Builds a [`ConfigError::Validation`] naming the config key that failed
/// and the plain-word validation detail.
pub(crate) fn build_validation_error(config_key: &str, validation_detail: &str) -> ConfigError {
    ConfigError::Validation {
        config_key: config_key.to_string(),
        validation_detail: validation_detail.to_string(),
    }
}

/// A KDL syntax error with the config file path attached. It holds the
/// [`kdl::KdlError`], which carries the source text and the error spans, and
/// `config_path` for the diagnostic header. The [`Diagnostic`] impl returns the
/// inner error's source code and sub-diagnostics, each with its own span, and a
/// rendered report points a caret at the failing line.
#[derive(Debug, Error)]
#[error("config parse error in {config_path}")]
pub struct ConfigParseDiagnostic {
    /// Path of the config file that failed to parse, for the header line.
    config_path: String,
    /// The underlying KDL parse error, carrying source text and spans.
    kdl_parse_error: KdlError,
}

impl ConfigParseDiagnostic {
    /// Builds a diagnostic from a KDL parse error and the file path it came from.
    pub fn from_kdl_error(config_path: &Path, kdl_parse_error: KdlError) -> Self {
        Self {
            config_path: config_path.display().to_string(),
            kdl_parse_error,
        }
    }

    /// The plain [`ConfigError::Parse`] for this diagnostic. Its detail is the
    /// first KDL sub-diagnostic as `line <L>, column <C>: <message>`, at the
    /// start of that sub-diagnostic's span, or the KDL error's own message
    /// ("Failed to parse KDL document") when it reported none.
    ///
    /// Example — `version 1\npane {` in `koshi.kdl` gives `config parse error
    /// in koshi.kdl: line 2, column 6: No closing '}' for child block`.
    #[must_use]
    pub fn build_config_error(&self) -> ConfigError {
        let parse_error_detail = match self.kdl_parse_error.diagnostics.first() {
            Some(kdl_diagnostic) => {
                let mut located_messages = format_problems_at_line_and_column(
                    &self.kdl_parse_error.input,
                    [(kdl_diagnostic.span.offset(), kdl_diagnostic)],
                );
                located_messages.remove(0)
            }
            None => self.kdl_parse_error.to_string(),
        };
        ConfigError::Parse {
            config_path: self.config_path.clone(),
            parse_error_detail,
        }
    }
}

/// Each problem of `problems`, a `(byte offset in source_text, message)` pair,
/// as `line <L>, column <C>: <message>`, in the order given. `L` and `C` are the
/// 1-based line and column of the character that starts at the byte offset:
/// `\n`, `\r`, and `\r\n` each end one line, and the column counts characters,
/// not bytes. An offset past the last character gives the position after it.
///
/// The offsets ascend: each one is at least the one before it. The scan passes
/// over `source_text` once for all the problems.
///
/// Example — in `a\nnode "é" {`, the problem at byte 12 with the message
/// `No closing '}' for child block` gives `line 2, column 10: No closing '}'
/// for child block`.
pub(crate) fn format_problems_at_line_and_column<ProblemMessage: Display>(
    source_text: &str,
    problems: impl IntoIterator<Item = (usize, ProblemMessage)>,
) -> Vec<String> {
    let mut scanned_byte_count = 0;
    let mut line_number = 1;
    let mut column_number = 1;
    let mut previous_character = None;
    let mut located_messages = Vec::new();
    for (byte_offset, problem_message) in problems {
        for character in source_text[scanned_byte_count..].chars() {
            if scanned_byte_count >= byte_offset {
                break;
            }
            match character {
                '\n' if previous_character == Some('\r') => {}
                '\n' | '\r' => {
                    line_number += 1;
                    column_number = 1;
                }
                _ => column_number += 1,
            }
            previous_character = Some(character);
            scanned_byte_count += character.len_utf8();
        }
        located_messages.push(format!(
            "line {line_number}, column {column_number}: {problem_message}"
        ));
    }
    located_messages
}

impl Diagnostic for ConfigParseDiagnostic {
    // Always `koshi::config::parse`.
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new("koshi::config::parse"))
    }

    // The inner `kdl` error's source text, which a rendered report highlights.
    fn source_code(&self) -> Option<&dyn SourceCode> {
        self.kdl_parse_error.source_code()
    }

    // The inner `kdl` error's own sub-diagnostics, each keeping its own span.
    fn related<'a>(&'a self) -> Option<Box<dyn Iterator<Item = &'a dyn Diagnostic> + 'a>> {
        self.kdl_parse_error.related()
    }
}

impl From<ConfigParseDiagnostic> for ConfigError {
    fn from(parse_diagnostic: ConfigParseDiagnostic) -> Self {
        parse_diagnostic.build_config_error()
    }
}

/// A config schema version that cannot be used by this build.
#[derive(Debug, Error, Diagnostic)]
pub enum ConfigVersionDiagnostic {
    /// Schema versions start at one.
    #[error("config schema version must be at least 1")]
    #[diagnostic(code(koshi::config::version))]
    TooOld,
    /// The file comes from a newer Koshi schema.
    #[error(
        "config schema version {declared_schema_version} is newer than this koshi supports ({supported_schema_version})"
    )]
    #[diagnostic(
        code(koshi::config::version),
        help("upgrade koshi to a build that understands this config")
    )]
    TooNew {
        /// The version declared in the config file.
        declared_schema_version: u32,
        /// The newest schema version this build supports.
        supported_schema_version: u32,
    },
}

/// Checks `declared_schema_version` against [`SCHEMA_VERSION`].
/// Every version from 1 through [`SCHEMA_VERSION`] is accepted.
///
/// # Errors
/// Returns a [`ConfigVersionDiagnostic`] when `declared_schema_version` is zero or newer than
/// [`SCHEMA_VERSION`].
pub fn validate_config_schema_version(
    declared_schema_version: u32,
) -> Result<(), ConfigVersionDiagnostic> {
    if declared_schema_version == 0 {
        Err(ConfigVersionDiagnostic::TooOld)
    } else if declared_schema_version > SCHEMA_VERSION {
        Err(ConfigVersionDiagnostic::TooNew {
            declared_schema_version,
            supported_schema_version: SCHEMA_VERSION,
        })
    } else {
        Ok(())
    }
}

/// A theme color value that is not a valid `#RRGGBB` hex string.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ColorParseError {
    /// The value did not have exactly six characters.
    #[error("color must be 6 hex digits (#RRGGBB), got {character_count}")]
    BadLength {
        /// The number of characters supplied.
        character_count: usize,
    },
    /// The value contained a character that is not a hex digit.
    #[error("color `{invalid_hex_text}` contains a non-hex digit")]
    BadDigit {
        /// The offending value.
        invalid_hex_text: String,
    },
}

#[cfg(test)]
mod tests;
