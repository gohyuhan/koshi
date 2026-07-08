//! Config domain errors. [`ConfigError`] is the flat, aggregate-wrapped failure
//! (classifies into [`DomainCategory::Config`]); [`ConfigParseDiagnostic`] is the
//! richer parse error that preserves KDL source spans for pretty rendering and
//! flattens into [`ConfigError`] when it enters the aggregate.

use std::path::Path;

use kdl::KdlError;
use koshi_core::error::{DomainCategory, DomainError, Severity};
use miette::{Diagnostic, SourceCode};
use thiserror::Error;

/// A failure in config discovery, parsing, or validation. Config problems are
/// recoverable: Koshi falls back to defaults and surfaces the issue to the user.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// No config file was found at the expected path.
    #[error("config file not found: {path}")]
    NotFound { path: String },
    /// The config file could not be parsed.
    #[error("config parse error in {path}: {detail}")]
    Parse { path: String, detail: String },
    /// The config parsed but failed schema validation.
    #[error("invalid config key `{key}`: {detail}")]
    Validation { key: String, detail: String },
}

impl DomainError for ConfigError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Config
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// A KDL syntax error with the config file path attached. Wraps the underlying
/// [`kdl::KdlError`] — which already carries the source text and error spans —
/// and adds `path` for the diagnostic header. The [`Diagnostic`] impl forwards
/// the source code and related sub-diagnostics to the inner error — where the
/// KDL crate carries each error's span — so a rendered report points a caret at
/// the offending line.
#[derive(Debug, Error)]
#[error("config parse error in {path}")]
pub struct ConfigParseDiagnostic {
    /// Path of the config file that failed to parse, for the header line.
    path: String,
    /// The underlying KDL parse error, carrying source text and spans.
    err: KdlError,
}

impl ConfigParseDiagnostic {
    /// Builds a diagnostic from a KDL parse `err` and the file `path` it came from.
    pub fn new(path: &Path, err: KdlError) -> Self {
        Self {
            path: path.display().to_string(),
            err,
        }
    }
}

impl Diagnostic for ConfigParseDiagnostic {
    fn code<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        Some(Box::new("koshi::config::parse"))
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        self.err.source_code()
    }

    fn related<'a>(&'a self) -> Option<Box<dyn Iterator<Item = &'a dyn Diagnostic> + 'a>> {
        self.err.related()
    }
}

impl From<ConfigParseDiagnostic> for ConfigError {
    fn from(diag: ConfigParseDiagnostic) -> Self {
        // The first sub-diagnostic carries the specific message (kdl's own
        // top-level Display is the generic "Failed to parse KDL document");
        // fall back to that generic form only when kdl reported none.
        let detail = match diag.err.diagnostics.first() {
            Some(d) => d.to_string(),
            None => diag.err.to_string(),
        };
        ConfigError::Parse {
            path: diag.path,
            detail,
        }
    }
}
