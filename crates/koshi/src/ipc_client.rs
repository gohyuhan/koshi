//! The CLI side of the control socket: find the session's endpoint, connect,
//! open with Hello, submit one command, and read back its result.
//!
//! The endpoint file in the private runtime directory advertises the
//! session's socket address and connection token; reading it is the
//! same-user proof the Hello presents. The Hello and the command are written
//! back to back before either reply is read, so a submission costs one round
//! trip.
//!
//! A session another local user started advertises no endpoint file here. It
//! is found by name in the machine-wide shared directory instead, and reached
//! with an empty token, because that session asks another user for none.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::discovery::SessionOverview;
use koshi_core::event::RejectReason;
use koshi_core::ids::{CommandId, SessionId, TabId};
use koshi_ipc::endpoint::{shared_socket_addr, EndpointFile};
use koshi_ipc::error::IpcError;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind,
    IpcResult, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use koshi_ipc::wire::{MaybeKnown, WireName};
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
pub(crate) fn submit_external_via_runtime_dir(
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
    overview_of(&read_endpoint(runtime_dir, session_id)?, session_id)
}

/// Ask the session `session_id` listening at `socket` to describe itself, as
/// a session another local user started: the address is the one the shared
/// directory advertised, and the token presented is empty.
pub fn fetch_foreign_overview(
    session_id: SessionId,
    socket: &str,
) -> Result<SessionOverview, CliError> {
    overview_of(&foreign_endpoint(socket.to_string()), session_id)
}

/// One Discovery exchange over `endpoint`, for the session `session_id`.
fn overview_of(
    endpoint: &EndpointFile,
    session_id: SessionId,
) -> Result<SessionOverview, CliError> {
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Discovery,
    };
    match exchange(endpoint, session_id, request)? {
        IpcResult::Overview(overview) => Ok(overview),
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(unexpected_reply(&other)),
    }
}

/// Ask the running session `session_id` to describe its layout: each tab's
/// split tree, and the rectangles each viewing client solves it to
/// ([`SessionLayout`]). `tab` narrows the answer to one tab; absent, every
/// tab is described.
///
/// Naming a `tab` the session no longer holds is a target failure, not an
/// empty answer: the session describes no tab, and this reports the tab as
/// missing. It is reachable when the tab closes between the caller resolving
/// it and the session answering.
///
/// A session whose build has no layout request refuses it two ways, and both
/// are reported as the version gap they are, naming what to do instead. A
/// session from this build or later names the kind it lacks
/// ([`UnsupportedKind`](IpcErrorCode::UnsupportedKind)); one older than the
/// tolerant wire cannot read the request at all
/// ([`MalformedRequest`](IpcErrorCode::MalformedRequest)).
pub fn fetch_layout(
    runtime_dir: &Path,
    session_id: SessionId,
    tab: Option<TabId>,
) -> Result<SessionLayout, CliError> {
    let endpoint = read_endpoint(runtime_dir, session_id)?;
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Layout { tab },
    };
    match exchange(&endpoint, session_id, request)? {
        IpcResult::Layout(layout) => match tab {
            Some(tab_id) if layout.tabs.is_empty() => Err(CliError::CommandRejected {
                reason: RejectReason::TargetNotFound,
                help: Some(format!("no running session has tab {tab_id}")),
            }),
            _ => Ok(layout),
        },
        IpcResult::Error(refusal)
            if matches!(
                refusal.code,
                IpcErrorCode::UnsupportedKind | IpcErrorCode::MalformedRequest
            ) =>
        {
            Err(CliError::IpcUnavailable {
                detail: "this session was started by an older koshi that cannot report its \
                         layout; restart the session to use `debug dump-layout`, or run \
                         `koshi debug dump-state`, which this session does answer"
                    .to_string(),
            })
        }
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
        .filter_map(|entry| session_id_of(entry.file_name().to_str()?, ".json"))
        .collect()
}

/// The session a file named `session-<uuid>` plus `suffix` names. Any other
/// name is `None`.
fn session_id_of(name: &str, suffix: &str) -> Option<SessionId> {
    let bare = name.strip_suffix(suffix)?.strip_prefix("session-")?;
    Some(SessionId::from_uuid(Uuid::parse_str(bare).ok()?))
}

/// The machine-wide directory holding the sessions other local users started,
/// or `None` while `allow-other-users` is off in `koshi.kdl` or the machine
/// reports no such directory.
///
/// `koshi.kdl` is read again on each call, so the answer is the one the file
/// holds at this moment.
#[must_use]
pub fn shared_base() -> Option<PathBuf> {
    let server = crate::config::server_config_now();
    if !server.allow_other_users {
        return None;
    }
    server
        .shared_sessions_dir
        .or_else(koshi_paths::shared_sessions_dir)
}

/// Every session another local user started that `shared_base` advertises, as
/// its id and the control-socket address reaching it, in no particular order.
///
/// This user's own sessions are left out, so a caller never asks its own
/// session for the empty token that session refuses. On Unix each user's
/// sockets sit in a subdirectory named after that user's id, and the one
/// named after the user owning `runtime_dir` is skipped. On Windows the
/// markers share one flat directory and name no user, so an id `runtime_dir`
/// also advertises is this user's own and is dropped.
///
/// A session is counted by its file name alone; whether anything still
/// listens behind it is the caller's probe to make. An unreadable directory
/// reads as no sessions.
#[must_use]
pub fn foreign_sessions(shared_base: &Path, runtime_dir: &Path) -> Vec<(SessionId, String)> {
    let Ok(entries) = std::fs::read_dir(shared_base) else {
        return Vec::new();
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let own = std::fs::metadata(runtime_dir)
            .map(|dir| dir.uid().to_string())
            .ok();
        entries
            .filter_map(Result::ok)
            .filter(|entry| Some(entry.file_name().to_string_lossy().into_owned()) != own)
            .flat_map(|entry| sockets_in(&entry.path()))
            .collect()
    }
    #[cfg(windows)]
    {
        let own = advertised_sessions(runtime_dir);
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let id = session_id_of(entry.file_name().to_str()?, "")?;
                (!own.contains(&id)).then(|| (id, shared_socket_addr(shared_base, id)))
            })
            .collect()
    }
}

/// The sessions one user's subdirectory of the shared directory advertises,
/// each as its id and the socket file reaching it. An unreadable
/// subdirectory reads as no sessions.
#[cfg(unix)]
fn sockets_in(user_dir: &Path) -> Vec<(SessionId, String)> {
    let Ok(entries) = std::fs::read_dir(user_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let id = session_id_of(entry.file_name().to_str()?, ".sock")?;
            Some((id, shared_socket_addr(user_dir, id)))
        })
        .collect()
}

/// How to reach the session another local user started at `socket`: the
/// address the shared directory advertised, and the empty token that session
/// asks another user for. That user's own endpoint file stays unread.
fn foreign_endpoint(socket: String) -> EndpointFile {
    EndpointFile {
        socket,
        token: ConnectionToken::new(""),
        pid: 0,
    }
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
        kind: IpcRequestKind::hello(endpoint.token.clone()),
    };
    connection.send(&hello).map_err(talk_failed)?;
    connection.send(&request).map_err(talk_failed)?;

    let hello_reply: IncomingResponse = connection.recv().map_err(talk_failed)?;
    match take_result(hello_reply)? {
        IpcResult::Hello { protocol_version } => settled_version(protocol_version)?,
        IpcResult::Error(refusal) => return Err(refused(&refusal)),
        other => return Err(unexpected_reply(&other)),
    }

    let reply: IncomingResponse = connection.recv().map_err(talk_failed)?;
    take_result(reply)
}

/// Check the version the session settled on against the range this build sent.
///
/// The session picks from the range the Hello named. A version outside that
/// range is not one this koshi offered, so the exchange stops here.
///
/// Example — this build asks for 2 to 2 and the reply names 3, so the verb
/// fails naming both.
pub(crate) fn settled_version(protocol_version: u32) -> Result<(), CliError> {
    if (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&protocol_version) {
        return Ok(());
    }
    Err(CliError::IpcUnavailable {
        detail: format!(
            "the session settled on protocol version {protocol_version}, \
             which is outside the {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION} this koshi asked for"
        ),
    })
}

/// The answer inside a response, or an error when the session named a result
/// this build does not have.
///
/// A result kind this build has no name for is a protocol violation, so the
/// command fails.
pub(crate) fn take_result(response: IncomingResponse) -> Result<IpcResult, CliError> {
    match response.result {
        MaybeKnown::Known(result) => Ok(result),
        MaybeKnown::Unknown { name } => Err(unexpected_name(&name)),
    }
}

/// How to reach `session_id`: the endpoint file in `runtime_dir`, or — for a
/// session another local user started — what the shared directory advertises
/// for it.
///
/// A session another local user started writes its endpoint file into that
/// user's own runtime directory, which this user may not read and does not
/// need: the shared directory names the socket, and the token presented is
/// empty. An id neither place holds means no running koshi advertises that
/// session.
pub(crate) fn read_endpoint(
    runtime_dir: &Path,
    session_id: SessionId,
) -> Result<EndpointFile, CliError> {
    let path = EndpointFile::path(runtime_dir, session_id);
    match EndpointFile::read(&path) {
        Ok(endpoint) => Ok(endpoint),
        Err(IpcError::EndpointFileMissing { .. }) => shared_base()
            .into_iter()
            .flat_map(|base| foreign_sessions(&base, runtime_dir))
            .find(|(id, _)| *id == session_id)
            .map(|(_, socket)| foreign_endpoint(socket))
            .ok_or_else(|| CliError::SessionNotFound {
                session: session_id.to_string(),
            }),
        Err(other) => Err(CliError::IpcUnavailable {
            detail: other.to_string(),
        }),
    }
}

/// Connect to the advertised socket. An address nothing listens on is a
/// leftover from a session that is gone, so it reports the session as not
/// running rather than a transport fault.
pub(crate) fn connect(
    endpoint: &EndpointFile,
    session_id: SessionId,
) -> Result<Connection, CliError> {
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
pub(crate) fn talk_failed(error: IpcError) -> CliError {
    CliError::IpcUnavailable {
        detail: error.to_string(),
    }
}

/// The server refused a request at the protocol level (bad token, version
/// mismatch, unreadable request).
pub(crate) fn refused(refusal: &IpcErrorPayload) -> CliError {
    CliError::IpcUnavailable {
        detail: refusal.message.clone(),
    }
}

/// The server answered with a result kind the request cannot produce —
/// a protocol violation, not a command outcome.
pub(crate) fn unexpected_reply(result: &IpcResult) -> CliError {
    unexpected_name(result.wire_name())
}

/// The same failure, named by the reply's wire name alone — for a reply this
/// build has no variant for.
pub(crate) fn unexpected_name(name: &str) -> CliError {
    CliError::IpcUnavailable {
        detail: format!("the session answered with an unexpected {name} reply"),
    }
}

#[cfg(test)]
mod tests;
