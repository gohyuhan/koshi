//! Plugin errors. Each variant reports [`DomainCategory::Plugin`] and
//! [`Severity::Recoverable`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

/// A failure loading or running a plugin. Every variant reports
/// [`Severity::Recoverable`].
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
    /// Returns [`DomainCategory::Plugin`].
    fn category(&self) -> DomainCategory {
        DomainCategory::Plugin
    }

    /// Returns [`Severity::Recoverable`].
    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

#[cfg(test)]
mod tests;
