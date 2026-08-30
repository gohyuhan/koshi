//! `koshi-error` — [`KoshiError`] wraps a config, CLI, IPC, PTY, terminal,
//! layout, plugin or storage error into one type. It keeps the category and
//! the severity of the wrapped error.

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

/// A config, CLI, IPC, PTY, terminal, layout, plugin or storage failure,
/// wrapped as one type. `Display`,
/// [`category`](KoshiError::category) and [`severity`](KoshiError::severity)
/// give the wrapped error's own values; `source` gives the wrapped error's
/// own `source`.
#[derive(Debug, Error)]
pub enum KoshiError {
    /// A failure in config discovery, parsing, or validation.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A failure the `koshi` binary terminates on: a usage problem, an
    /// unreachable runtime endpoint, or a runtime or action failure.
    #[error(transparent)]
    Cli(#[from] CliError),
    /// A failure on the control channel.
    #[error(transparent)]
    Ipc(#[from] IpcError),
    /// A failure spawning or driving a child PTY.
    #[error(transparent)]
    Pty(#[from] PtyError),
    /// A failure in terminal emulation.
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    /// A failure in the layout engine.
    #[error(transparent)]
    Layout(#[from] LayoutError),
    /// A failure loading or running a plugin.
    #[error(transparent)]
    Plugin(#[from] PluginError),
    /// A failure persisting or loading state.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl KoshiError {
    /// The wrapped error of the current variant, as a [`DomainError`].
    fn inner(&self) -> &dyn DomainError {
        match self {
            KoshiError::Config(e) => e,
            KoshiError::Cli(e) => e,
            KoshiError::Ipc(e) => e,
            KoshiError::Pty(e) => e,
            KoshiError::Terminal(e) => e,
            KoshiError::Layout(e) => e,
            KoshiError::Plugin(e) => e,
            KoshiError::Storage(e) => e,
        }
    }
}

impl DomainError for KoshiError {
    /// The wrapped error's category.
    fn category(&self) -> DomainCategory {
        self.inner().category()
    }

    /// The wrapped error's severity.
    fn severity(&self) -> Severity {
        self.inner().severity()
    }
}

#[cfg(test)]
mod tests;
