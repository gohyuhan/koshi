//! `koshi-error` — [`KoshiError`] wraps any crate's domain error into one
//! type. It keeps the category and the severity of the wrapped error.

use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

use koshi_config::error::ConfigError;
use koshi_ipc::error::IpcError;
use koshi_layout::error::LayoutError;
use koshi_link::error::CliError;
use koshi_plugin_host::error::PluginError;
use koshi_pty::error::PtyError;
use koshi_storage::error::StorageError;
use koshi_terminal::error::TerminalError;

/// Any domain failure, wrapped as one type. `Display`,
/// [`category`](KoshiError::category) and [`severity`](KoshiError::severity)
/// all forward to the wrapped error.
#[derive(Debug, Error)]
pub enum KoshiError {
    /// Configuration parse or validation failure.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// CLI argument or parsing failure.
    #[error(transparent)]
    Cli(#[from] CliError),
    /// IPC protocol or message failure.
    #[error(transparent)]
    Ipc(#[from] IpcError),
    /// PTY creation or control failure.
    #[error(transparent)]
    Pty(#[from] PtyError),
    /// Terminal state or rendering failure.
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    /// Layout tree or geometry failure.
    #[error(transparent)]
    Layout(#[from] LayoutError),
    /// Plugin load, init, or execution failure.
    #[error(transparent)]
    Plugin(#[from] PluginError),
    /// Session or event log storage failure.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl DomainError for KoshiError {
    /// The wrapped error's category.
    fn category(&self) -> DomainCategory {
        match self {
            KoshiError::Config(e) => e.category(),
            KoshiError::Cli(e) => e.category(),
            KoshiError::Ipc(e) => e.category(),
            KoshiError::Pty(e) => e.category(),
            KoshiError::Terminal(e) => e.category(),
            KoshiError::Layout(e) => e.category(),
            KoshiError::Plugin(e) => e.category(),
            KoshiError::Storage(e) => e.category(),
        }
    }

    /// The wrapped error's severity.
    fn severity(&self) -> Severity {
        match self {
            KoshiError::Config(e) => e.severity(),
            KoshiError::Cli(e) => e.severity(),
            KoshiError::Ipc(e) => e.severity(),
            KoshiError::Pty(e) => e.severity(),
            KoshiError::Terminal(e) => e.severity(),
            KoshiError::Layout(e) => e.severity(),
            KoshiError::Plugin(e) => e.severity(),
            KoshiError::Storage(e) => e.severity(),
        }
    }
}

#[cfg(test)]
mod tests;
