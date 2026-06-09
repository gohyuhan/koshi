//! Aggregate error. [`TileError`] wraps any crate's domain error so the runtime
//! and diagnostics can handle one type while preserving its category and
//! severity (`TILE_12`). It lives here, not in `tile-core`, because wrapping the
//! concrete per-crate errors would otherwise cycle back through `tile-core`.

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
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error(transparent)]
    Ipc(#[from] IpcError),
    #[error(transparent)]
    Pty(#[from] PtyError),
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error(transparent)]
    Plugin(#[from] PluginError),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl DomainError for TileError {
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
