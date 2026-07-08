//! CLI binary error and its exit-code mapping.
//!
//! [`CliError`](crate::error::CliError) enumerates the failure classes the
//! `koshi` binary terminates on. The `From<&CliError> for CliExitCode` impl is
//! the single
//! error-to-exit-code table: usage → 2, IPC-unavailable → 4, runtime → 1.
//! Success is exit 0, and a rejected dispatched command maps through
//! [`CliExitCode::for_result`](koshi_core::command::CliExitCode::for_result).

use koshi_core::command::CliExitCode;
use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

/// A failure the `koshi` binary terminates on: a usage problem, an unreachable
/// runtime endpoint, or a runtime/action error. Each reports through a
/// [`CliExitCode`] and exits without affecting session state.
#[derive(Debug, Error)]
pub enum CliError {
    /// The subcommand is not recognized.
    #[error("unknown command: {name}")]
    UnknownCommand { name: String },
    /// The named action is not in the action registry.
    #[error("unknown action: {name}")]
    UnknownAction { name: String },
    /// Arguments were missing or invalid for the chosen command.
    #[error("invalid arguments: {detail}")]
    InvalidArgs { detail: String },
    /// The runtime IPC endpoint could not be reached.
    #[error("IPC unavailable: {detail}")]
    IpcUnavailable { detail: String },
    /// A runtime or action error surfaced while executing.
    #[error("{detail}")]
    Runtime { detail: String },
}

impl DomainError for CliError {
    fn category(&self) -> DomainCategory {
        match self {
            CliError::UnknownCommand { .. }
            | CliError::UnknownAction { .. }
            | CliError::InvalidArgs { .. } => DomainCategory::Cli,
            CliError::IpcUnavailable { .. } => DomainCategory::Ipc,
            CliError::Runtime { .. } => DomainCategory::Session,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// The single error-to-exit-code table: every [`CliError`] class maps to the
/// deterministic [`CliExitCode`] the binary reports to the OS. Usage errors
/// exit 2, an unreachable IPC endpoint exits 4, and a runtime or action error
/// exits 1.
impl From<&CliError> for CliExitCode {
    fn from(err: &CliError) -> Self {
        match err {
            CliError::UnknownCommand { .. }
            | CliError::UnknownAction { .. }
            | CliError::InvalidArgs { .. } => CliExitCode::UsageOrConfig,
            CliError::IpcUnavailable { .. } => CliExitCode::IpcUnavailable,
            CliError::Runtime { .. } => CliExitCode::RuntimeAction,
        }
    }
}

#[cfg(test)]
mod tests;
