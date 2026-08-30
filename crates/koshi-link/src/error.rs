//! CLI binary error and its exit-code mapping.
//!
//! [`CliError`](crate::error::CliError) enumerates the failure classes the
//! `koshi` binary terminates on. The `From<&CliError> for CliExitCode` impl
//! below is the single error-to-exit-code table. Success is exit 0.

use koshi_core::command::CliExitCode;
use koshi_core::error::{DomainCategory, DomainError, Severity};
use koshi_core::event::RejectReason;
use thiserror::Error;

/// A failure the `koshi` binary terminates on. Each variant maps to the
/// [`CliExitCode`] the process exits with through the `From<&CliError>` impl
/// below.
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
    /// The described key sequence is not bound in any mode.
    #[error("nothing is bound on `{sequence}` in any mode")]
    UnboundKey { sequence: String },
    /// A keybinding file dry-run found problems.
    #[error("keybinding file {path} failed validation")]
    InvalidKeymapFile { path: String },
    /// A config command could not read, validate, explain, or migrate config.
    #[error("config failed: {detail}")]
    Config { detail: String },
    /// The `KOSHI` marker is set but the rest of the in-session environment
    /// is missing or malformed.
    #[error("broken in-session environment: {detail}")]
    InSessionEnv { detail: String },
    /// The runtime IPC endpoint could not be reached.
    #[error("IPC unavailable: {detail}")]
    IpcUnavailable { detail: String },
    /// The named (or in-session) session is not running: nothing advertises
    /// its endpoint, or nothing listens behind the advertised socket.
    #[error("session {session} is not running")]
    SessionNotFound { session: String },
    /// No running koshi advertises any session, so there is nothing for an
    /// external command to target.
    #[error("no koshi session is running")]
    NoSessions,
    /// The session refused the dispatched command.
    #[error("{}", rejection_message(*.reason, .help.as_deref()))]
    CommandRejected {
        /// Why the session rejected it.
        reason: RejectReason,
        /// The session's hint for resolving the rejection, when it sent one.
        help: Option<String>,
    },
    /// A runtime or action error surfaced while executing.
    #[error("{detail}")]
    Runtime { detail: String },
    /// A self-update check or install failed.
    #[error("update failed: {detail}")]
    Update { detail: String },
}

/// The message for a rejected command: `reason`, then `help` on the next line
/// indented by two spaces when the session sent one.
///
/// [`RejectReason::Unauthorized`] with `Some("attach first")` gives
/// `"command not permitted\n  attach first"`.
fn rejection_message(reason: RejectReason, help: Option<&str>) -> String {
    match help {
        Some(help) => format!("{reason}\n  {help}"),
        None => reason.to_string(),
    }
}

impl DomainError for CliError {
    fn category(&self) -> DomainCategory {
        match self {
            CliError::UnknownCommand { .. }
            | CliError::UnknownAction { .. }
            | CliError::InvalidArgs { .. }
            | CliError::UnboundKey { .. }
            | CliError::InvalidKeymapFile { .. }
            | CliError::Config { .. }
            | CliError::InSessionEnv { .. } => DomainCategory::Cli,
            CliError::IpcUnavailable { .. } => DomainCategory::Ipc,
            CliError::SessionNotFound { .. }
            | CliError::NoSessions
            | CliError::Runtime { .. }
            | CliError::CommandRejected { .. }
            | CliError::Update { .. } => DomainCategory::Session,
        }
    }

    fn severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// The single error-to-exit-code table: every [`CliError`] class maps to the
/// [`CliExitCode`] the binary reports to the OS. A usage or config problem
/// exits 2, a session that is not running exits 3, an unreachable IPC endpoint
/// exits 4, and a runtime error, a rejected command, or a failed update exits
/// 1.
impl From<&CliError> for CliExitCode {
    fn from(err: &CliError) -> Self {
        match err {
            CliError::UnknownCommand { .. }
            | CliError::UnknownAction { .. }
            | CliError::InvalidArgs { .. }
            | CliError::UnboundKey { .. }
            | CliError::InvalidKeymapFile { .. }
            | CliError::Config { .. }
            | CliError::InSessionEnv { .. } => CliExitCode::UsageOrConfig,
            CliError::IpcUnavailable { .. } => CliExitCode::IpcUnavailable,
            CliError::SessionNotFound { .. } | CliError::NoSessions => CliExitCode::SessionNotFound,
            CliError::Runtime { .. }
            | CliError::CommandRejected { .. }
            | CliError::Update { .. } => CliExitCode::RuntimeAction,
        }
    }
}

#[cfg(test)]
mod tests;
