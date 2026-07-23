//! The CLI side of the control socket: find the session's endpoint, connect,
//! open with Hello, submit one command, and read back its result.
//!
//! The endpoint file in the private runtime directory advertises the
//! session's socket address and connection token; reading it is the
//! same-user proof the Hello presents. The Hello and the command are written
//! back to back before either reply is read, so a submission costs one round
//! trip.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::discovery::SessionOverview;
use koshi_core::ids::{CommandId, SessionId};
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{
    IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse, IpcResult, PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use uuid::Uuid;

use crate::error::CliError;
use crate::in_session::InSessionContext;

/// The private runtime directory holding every endpoint file, or
/// [`CliError::IpcUnavailable`] when the machine has none.
pub fn runtime_dir() -> Result<PathBuf, CliError> {
    koshi_paths::runtime_dir().ok_or_else(|| CliError::IpcUnavailable {
        detail: "no runtime directory found".to_string(),
    })
}

/// Submit `command` to the session this CLI runs inside and hand back the
/// dispatcher's result.
///
/// Reads the session's endpoint file, connects, writes the Hello and the
/// command back to back, and reads the two replies in order. A missing
/// endpoint file or a socket nothing listens on reports the session as not
/// running ([`CliError::SessionNotFound`]); every other failure to talk is
/// [`CliError::IpcUnavailable`]. The result itself — applied or rejected —
/// comes back for the caller to map to an exit code.
pub fn submit_in_session(
    context: &InSessionContext,
    command: Command,
) -> Result<CommandResult, CliError> {
    submit_via_runtime_dir(&runtime_dir()?, context, command)
}

/// Submit `command` to the running session `session_id` as an external
/// invocation — a `koshi` command typed outside any pane, or inside a pane
/// but targeting another session. Same exchange and error mapping as
/// [`submit_in_session`]; the envelope's source is
/// [`CommandSource::external_cli`], so the runtime resolves defaults through
/// the target session's acting client rather than an issuing pane.
pub fn submit_external(session_id: SessionId, command: Command) -> Result<CommandResult, CliError> {
    submit_external_via_runtime_dir(&runtime_dir()?, session_id, command)
}

/// [`submit_in_session`] against an explicit runtime directory: the whole
/// exchange, with the endpoint lookup rooted where the caller says.
fn submit_via_runtime_dir(
    runtime_dir: &Path,
    context: &InSessionContext,
    command: Command,
) -> Result<CommandResult, CliError> {
    let endpoint = read_endpoint(runtime_dir, context.session_id)?;
    let source = CommandSource::in_session_cli(
        context.session_id,
        context.client_id,
        context.pane_id,
        PathBuf::from(&endpoint.socket),
    );
    submit_envelope(&endpoint, context.session_id, source, command)
}

/// [`submit_external`] against an explicit runtime directory: the whole
/// exchange, with the endpoint lookup rooted where the caller says.
fn submit_external_via_runtime_dir(
    runtime_dir: &Path,
    session_id: SessionId,
    command: Command,
) -> Result<CommandResult, CliError> {
    let endpoint = read_endpoint(runtime_dir, session_id)?;
    let source = CommandSource::external_cli(Some(session_id));
    submit_envelope(&endpoint, session_id, source, command)
}

/// Fill a pane-creating command's unset working directory with this CLI
/// process's own, captured here at send time: the CLI inherited it from the
/// shell it was typed in, so the new pane opens where the command was run.
/// A command that already names a directory is left alone, and every other
/// command carries none.
fn capture_cwd(command: Command) -> Command {
    let mut command = command;
    let cwd = match &mut command {
        Command::NewPane(args) => &mut args.cwd,
        Command::NewTab(args) => &mut args.cwd,
        Command::RunCommandPane(args) => &mut args.cwd,
        _ => return command,
    };
    if cwd.is_none() {
        *cwd = std::env::current_dir().ok();
    }
    command
}

/// One command submission over `endpoint`: connect, pipeline Hello and the
/// enveloped command, read both replies in order. A pane-creating command
/// with no directory of its own gets this process's ([`capture_cwd`]).
fn submit_envelope(
    endpoint: &EndpointFile,
    session_id: SessionId,
    source: CommandSource,
    command: Command,
) -> Result<CommandResult, CliError> {
    let command = capture_cwd(command);
    let envelope = CommandEnvelope::new(CommandId::new(), source, SystemTime::now(), command);
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
    };
    match exchange(endpoint, session_id, request)? {
        IpcResult::CommandResult(result) => Ok(result),
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(unexpected_reply(&other)),
    }
}

/// Ask the running session `session_id` to describe itself in full: tabs,
/// panes, and attached clients ([`SessionOverview`]). The routing layer uses
/// the answer to resolve names to ids and to find which session owns an
/// explicitly named pane, tab, or client.
pub fn fetch_overview(
    runtime_dir: &Path,
    session_id: SessionId,
) -> Result<SessionOverview, CliError> {
    let endpoint = read_endpoint(runtime_dir, session_id)?;
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Discovery,
    };
    match exchange(&endpoint, session_id, request)? {
        IpcResult::Overview(overview) => Ok(overview),
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(unexpected_reply(&other)),
    }
}

/// Every session with an endpoint file in `runtime_dir`, in no particular
/// order. A file is counted by its name alone (`session-<uuid>.json`);
/// whether anything still listens behind it is the caller's probe to make.
/// An unreadable directory reads as no sessions.
pub fn advertised_sessions(runtime_dir: &Path) -> Vec<SessionId> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let stem = name.strip_suffix(".json")?;
            let uuid = stem
                .strip_prefix("session-")
                .and_then(|bare| Uuid::parse_str(bare).ok())?;
            Some(SessionId::from_uuid(uuid))
        })
        .collect()
}

/// Connect to `endpoint`, pipeline the Hello and `request` back to back, and
/// read both replies in order — the server answers every request in order,
/// so this costs one round trip. Returns `request`'s result; a failed Hello
/// is an error.
fn exchange(
    endpoint: &EndpointFile,
    session_id: SessionId,
    request: IpcRequest,
) -> Result<IpcResult, CliError> {
    let mut connection = connect(endpoint, session_id)?;
    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            protocol_version: PROTOCOL_VERSION,
            token: endpoint.token.clone(),
        },
    };
    connection.send(&hello).map_err(talk_failed)?;
    connection.send(&request).map_err(talk_failed)?;

    let hello_reply: IpcResponse = connection.recv().map_err(talk_failed)?;
    match hello_reply.result {
        IpcResult::Hello => {}
        IpcResult::Error(refusal) => return Err(refused(&refusal)),
        other => return Err(unexpected_reply(&other)),
    }

    let reply: IpcResponse = connection.recv().map_err(talk_failed)?;
    Ok(reply.result)
}

/// Read the endpoint file for `session_id`. A missing file means no running
/// koshi advertises that session.
fn read_endpoint(runtime_dir: &Path, session_id: SessionId) -> Result<EndpointFile, CliError> {
    let path = EndpointFile::path(runtime_dir, session_id);
    EndpointFile::read(&path).map_err(|error| match error {
        IpcError::EndpointFileMissing { .. } => CliError::SessionNotFound {
            session: session_id.to_string(),
        },
        other => CliError::IpcUnavailable {
            detail: other.to_string(),
        },
    })
}

/// Connect to the advertised socket. An address nothing listens on is a
/// leftover from a session that is gone, so it reports the session as not
/// running rather than a transport fault.
fn connect(endpoint: &EndpointFile, session_id: SessionId) -> Result<Connection, CliError> {
    Connection::connect(&endpoint.socket).map_err(|error| match error {
        IpcError::NoListener { .. } => CliError::SessionNotFound {
            session: session_id.to_string(),
        },
        other => CliError::IpcUnavailable {
            detail: other.to_string(),
        },
    })
}

/// A transport failure mid-exchange: the endpoint was reachable but the
/// conversation could not finish.
fn talk_failed(error: IpcError) -> CliError {
    CliError::IpcUnavailable {
        detail: error.to_string(),
    }
}

/// The server refused a request at the protocol level (bad token, version
/// mismatch, unreadable request).
fn refused(refusal: &IpcErrorPayload) -> CliError {
    CliError::IpcUnavailable {
        detail: refusal.message.clone(),
    }
}

/// The server answered with a result kind the request cannot produce —
/// a protocol violation, not a command outcome.
fn unexpected_reply(result: &IpcResult) -> CliError {
    let kind = match result {
        IpcResult::Hello => "Hello",
        IpcResult::CommandResult(_) => "CommandResult",
        IpcResult::Overview(_) => "Overview",
        IpcResult::Error(_) => "Error",
    };
    CliError::IpcUnavailable {
        detail: format!("the session answered with an unexpected {kind} reply"),
    }
}

#[cfg(test)]
mod tests;
