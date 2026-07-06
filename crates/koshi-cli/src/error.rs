//! CLI domain error. Classifies into [`koshi_core::error::DomainCategory::Cli`].

use koshi_core::error::{DomainCategory, DomainError, Severity};
use thiserror::Error;

/// A failure parsing or dispatching a CLI invocation. Usage problems are
/// recoverable: the CLI reports them and exits without affecting a session.
#[derive(Debug, Error)]
pub enum CliError {
    /// The subcommand is not recognized.
    #[error("unknown command: {name}")]
    UnknownCommand { name: String },
    /// Arguments were missing or invalid for the chosen command.
    #[error("invalid arguments: {detail}")]
    InvalidArgs { detail: String },
}

impl DomainError for CliError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Cli
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}
