//! Config domain errors.
//!
//! [`ConfigError`] is the plain error enum other code matches on; it
//! classifies into [`DomainCategory::Config`] when it joins koshi's
//! crate-wide aggregate error type. [`ConfigParseDiagnostic`] is a richer
//! parse error that keeps the original KDL source text and the byte span of
//! the failure; a rendered report points a caret at the bad line. It
//! flattens down into a plain [`ConfigError::Parse`] once it joins the
//! aggregate. [`ConfigVersionDiagnostic`] reports a declared schema version
//! that is zero or newer than this build supports.
//! [`ColorParseError`] reports a theme color value that is not valid
//! `#RRGGBB` hex.

use std::path::Path;

use kdl::KdlError;
use koshi_core::error::{DomainCategory, DomainError, Severity};
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
        config_path: String,
        parse_error_detail: String,
    },
    /// The config parsed but failed schema validation.
    #[error("invalid config key `{config_key}`: {validation_detail}")]
    Validation {
        config_key: String,
        validation_detail: String,
    },
}

impl DomainError for ConfigError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Config
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// Builds a [`ConfigError::Validation`] naming the config key that failed
/// and the plain-word validation detail.
pub(crate) fn build_validation_error(config_key: &str, validation_detail: &str) -> ConfigError {
    ConfigError::Validation {
        config_key: config_key.to_string(),
        validation_detail: validation_detail.to_string(),
    }
}

/// A KDL syntax error with the config file path attached. Wraps the underlying
/// [`kdl::KdlError`] — which already carries the source text and error spans —
/// and adds `path` for the diagnostic header. The [`Diagnostic`] impl forwards
/// the source code and related sub-diagnostics to the inner error — where the
/// KDL crate carries each error's span — so a rendered report points a caret at
/// the offending line.
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
        // `detail` is the first sub-diagnostic's message, or the kdl error's
        // own Display ("Failed to parse KDL document") when it reported none.
        let parse_error_detail = match parse_diagnostic.kdl_parse_error.diagnostics.first() {
            Some(kdl_diagnostic) => kdl_diagnostic.to_string(),
            None => parse_diagnostic.kdl_parse_error.to_string(),
        };
        ConfigError::Parse {
            config_path: parse_diagnostic.config_path,
            parse_error_detail,
        }
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
