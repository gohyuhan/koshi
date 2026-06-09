//! Plugin domain error. Classifies into [`DomainCategory::Plugin`] (`TILE_12`).

use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};

/// A failure loading or running a plugin. Recoverable: a failed plugin is
/// isolated and disabled without crashing the session (`TILE_12`).
#[derive(Debug, Error)]
pub enum PluginError {
    /// The plugin module could not be loaded or instantiated.
    #[error("failed to load plugin `{name}`: {detail}")]
    Load { name: String, detail: String },
    /// The plugin trapped or errored during execution.
    #[error("plugin `{name}` runtime error: {detail}")]
    Runtime { name: String, detail: String },
}

impl DomainError for PluginError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Plugin
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}
