//! CLI binary error and its exit-code mapping.
//!
//! [`CliError`] enumerates the failure classes the
//! `koshi` binary terminates on. The `From<&CliError> for CliExitCode` impl
//! below is the single error-to-exit-code table. Success is exit 0.

use koshi_core::command::CliExitCode;
use koshi_core::event::RejectReason;
use thiserror::Error;

/// A failure the `koshi` binary terminates on. Each variant maps to the
/// [`CliExitCode`] the process exits with through the `From<&CliError>` impl
/// below.
#[derive(Debug, Error)]
pub enum CliError {
    /// The named action is not in the action registry.
    #[error("unknown action: {action_name}")]
    UnknownAction { action_name: String },
    /// Arguments were missing or invalid for the chosen command.
    #[error("invalid arguments: {detail}")]
    InvalidArgs { detail: String },
    /// The described key sequence is not bound in any mode.
    #[error("nothing is bound on `{key_sequence_text}` in any mode")]
    UnboundKey { key_sequence_text: String },
    /// A keybinding file dry-run found problems.
    #[error("keybinding file {keybinding_file_path} failed validation")]
    InvalidKeybindingFile { keybinding_file_path: String },
    /// A config command failed, or a service could not migrate config before startup.
    #[error("config failed: {detail}")]
    Config { detail: String },
    /// The `KOSHI` marker is set but the rest of the in-session environment
    /// is missing or malformed.
    #[error("broken in-session environment: {detail}")]
    InSessionEnv { detail: String },
    /// The runtime IPC endpoint could not be reached.
    #[error("IPC unavailable: {detail}")]
    IpcUnavailable { detail: String },
    /// The peer refused the Hello: none of the protocol versions it speaks is
    /// one this build speaks. `detail` is the peer's own sentence.
    #[error("IPC unavailable: {detail}")]
    ProtocolVersionRefused { detail: String },
    /// The peer refused the Hello: the connection token presented is not the
    /// one it accepts. `detail` is the peer's own sentence.
    #[error("IPC unavailable: {detail}")]
    ConnectionTokenRefused { detail: String },
    /// The server was started by koshi 0.1.0 to 0.4.0, which reads no frame
    /// this build writes. `detail` is the sentence the read failure gave.
    #[error("IPC unavailable: {detail}; the user who started it runs: koshi restart-servers")]
    PreviousReleaseServer { detail: String },
    /// The session did not finish answering by the deadline the caller gave
    /// the exchange.
    #[error("IPC unavailable: the session did not answer in time")]
    SessionAnswerTimedOut,
    /// The named (or in-session) session is not running: nothing advertises
    /// its endpoint, or nothing listens behind the advertised socket.
    #[error("session {session_name} is not running")]
    SessionNotFound { session_name: String },
    /// No running koshi advertises any session, so there is nothing for an
    /// external command to target.
    #[error("no koshi session is running")]
    NoSessions,
    /// The session refused the dispatched command.
    #[error("{}", format_rejection_message(*.reason, .help.as_deref()))]
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
fn format_rejection_message(reason: RejectReason, help: Option<&str>) -> String {
    match help {
        Some(help) => format!("{reason}\n  {help}"),
        None => reason.to_string(),
    }
}

/// The single error-to-exit-code table: every [`CliError`] class maps to the
/// [`CliExitCode`] the binary reports to the OS. A usage or config problem
/// exits 2, a session that is not running exits 3, an unreachable IPC
/// endpoint, a refused protocol version, a refused connection token, a server
/// that koshi 0.1.0 to 0.4.0 started, or a session that did not answer in time
/// exits 4, and a runtime error, a rejected command, or a failed update exits
/// 1.
impl From<&CliError> for CliExitCode {
    fn from(cli_error: &CliError) -> Self {
        match cli_error {
            CliError::UnknownAction { .. }
            | CliError::InvalidArgs { .. }
            | CliError::UnboundKey { .. }
            | CliError::InvalidKeybindingFile { .. }
            | CliError::Config { .. }
            | CliError::InSessionEnv { .. } => CliExitCode::UsageOrConfig,
            CliError::IpcUnavailable { .. }
            | CliError::ProtocolVersionRefused { .. }
            | CliError::ConnectionTokenRefused { .. }
            | CliError::PreviousReleaseServer { .. }
            | CliError::SessionAnswerTimedOut => CliExitCode::IpcUnavailable,
            CliError::SessionNotFound { .. } | CliError::NoSessions => CliExitCode::SessionNotFound,
            CliError::Runtime { .. }
            | CliError::CommandRejected { .. }
            | CliError::Update { .. } => CliExitCode::RuntimeAction,
        }
    }
}

#[cfg(test)]
mod tests;
