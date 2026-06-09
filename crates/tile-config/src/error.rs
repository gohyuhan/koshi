//! Config domain error. Classifies into [`DomainCategory::Config`] (`TILE_12`).

use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};

/// A failure in config discovery, parsing, or validation. Config problems are
/// recoverable: Tile falls back to defaults and surfaces the issue to the user.
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
