//! `tile-error` — the aggregate [`TileError`] wraps any crate's domain error so
//! the runtime and diagnostics can handle one type while preserving its
//! category and severity. It lives in its own crate because wrapping the
//! concrete per-crate errors needs those crates as dependencies, which
//! `tile-core` (a dependency of every crate) cannot take on.

use thiserror::Error;
use tile_core::error::{DomainCategory, DomainError, Severity};

use tile_cli::error::CliError;
use tile_config::error::ConfigError;
use tile_ipc::error::IpcError;
use tile_layout::error::LayoutError;
use tile_plugin_host::error::PluginError;
use tile_pty::error::PtyError;
use tile_storage::error::StorageError;
use tile_terminal::error::TerminalError;

/// Any domain failure, wrapped for uniform handling. Display is transparent to
/// the wrapped error; [`category`](TileError::category) and
/// [`severity`](TileError::severity) delegate to it.
#[derive(Debug, Error)]
pub enum TileError {
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

impl DomainError for TileError {
    /// The error's domain category, delegated from the wrapped error.
    fn category(&self) -> DomainCategory {
        match self {
            TileError::Config(e) => e.category(),
            TileError::Cli(e) => e.category(),
            TileError::Ipc(e) => e.category(),
            TileError::Pty(e) => e.category(),
            TileError::Terminal(e) => e.category(),
            TileError::Layout(e) => e.category(),
            TileError::Plugin(e) => e.category(),
            TileError::Storage(e) => e.category(),
        }
    }

    /// The error's severity level, delegated from the wrapped error.
    fn severity(&self) -> Severity {
        match self {
            TileError::Config(e) => e.severity(),
            TileError::Cli(e) => e.severity(),
            TileError::Ipc(e) => e.severity(),
            TileError::Pty(e) => e.severity(),
            TileError::Terminal(e) => e.severity(),
            TileError::Layout(e) => e.severity(),
            TileError::Plugin(e) => e.severity(),
            TileError::Storage(e) => e.severity(),
        }
    }
}

#[cfg(test)]
mod tests;
