//! Plugin domain error: [`PluginError`]. Every variant classifies as
//! [`DomainCategory::Plugin`].

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
    /// Always [`DomainCategory::Plugin`].
    fn category(&self) -> DomainCategory {
        DomainCategory::Plugin
    }

    /// Always [`Severity::Recoverable`].
    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

#[cfg(test)]
mod tests;
