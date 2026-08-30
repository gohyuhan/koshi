//! The CLI side of the control socket: find the session's endpoint, connect,
//! open with Hello, submit one command, and read back its result.
//!
//! The endpoint file in the private runtime directory advertises the
//! session's socket address and connection token; reading it is the
//! same-user proof the Hello presents. The Hello and the command are written
//! back to back before either reply is read, so a submission costs one round
//! trip. A command naming a target client is the exception: the Hello answer
//! is read first, and a session that settled below protocol version 3 is
//! refused before the command is written.
//!
//! A session another local user started advertises no endpoint file here. It
//! is found by name in the machine-wide shared directory instead, and reached
//! with the empty token that session asks another user for.
//!
//! Asking a running session to restart is one more such exchange. A session
//! that is not listening reads as `NotRunning` rather than an error.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use koshi_core::command::{Command, CommandEnvelope, CommandResult, CommandSource};
use koshi_core::discovery::SessionOverview;
use koshi_core::event::RejectReason;
use koshi_core::ids::{ClientId, CommandId, SessionId, TabId};
use koshi_core::recent_event::RecentEvent;
use koshi_ipc::endpoint::{shared_socket_addr, EndpointFile, RESUME_SUFFIX};
use koshi_ipc::error::IpcError;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcErrorCode, IpcRequest, IpcRequestKind, IpcResult,
};
use koshi_ipc::transport::Connection;
use uuid::Uuid;

use crate::error::CliError;
use crate::in_session::InSessionContext;
use crate::talk::{self, refused, talk_failed};

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
/// comes back for the caller to map to an exit code, with a rejection's hint
/// filtered by [`sanitize_reported_text`](koshi_core::text::sanitize_reported_text).
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
///
/// `client_id` is the client the command acts for, and rides on the source.
/// Naming one costs a second round trip: the session's Hello answer is read
/// before the command is written, and a session that settled below protocol
/// version 3 is refused with [`CliError::IpcUnavailable`]. `None` names no
/// client and keeps the exchange at one round trip.
pub fn submit_external(
    session_id: SessionId,
    client_id: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    submit_external_via_runtime_dir(&runtime_dir()?, session_id, client_id, command)
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
    submit_envelope(&endpoint, context.session_id, source, command, false)
}

/// [`submit_external`] against an explicit runtime directory: the whole
/// exchange, with the endpoint lookup rooted where the caller says.
/// `client_id` rides on the source and costs the second round trip
/// [`submit_external`] describes.
pub fn submit_external_via_runtime_dir(
    runtime_dir: &Path,
    session_id: SessionId,
    client_id: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    let endpoint = read_endpoint(runtime_dir, session_id)?;
    let source = CommandSource::external_cli(Some(session_id), client_id);
    submit_envelope(&endpoint, session_id, source, command, client_id.is_some())
}

/// Fill a pane-creating command's unset working directory with this CLI
/// process's own, read here at send time, so the new pane opens where the
/// command was run. A command that already names a directory is left alone,
/// and every other command carries none.
fn capture_cwd(mut command: Command) -> Command {
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

/// One command submission over `endpoint`: connect, send the Hello and the
/// enveloped command, and read the command's result. A pane-creating command
/// with no directory of its own gets this process's ([`capture_cwd`]), and a
/// rejection's hint is filtered by
/// [`filter_rejection_hint`](crate::talk::filter_rejection_hint).
///
/// `names_client` goes to [`exchange`] unchanged: `false` writes the Hello and
/// the command back to back, and `true` refuses a session that settled below
/// [`TARGET_CLIENT_PROTOCOL`](crate::talk::TARGET_CLIENT_PROTOCOL) before the
/// command is written.
fn submit_envelope(
    endpoint: &EndpointFile,
    session_id: SessionId,
    source: CommandSource,
    command: Command,
    names_client: bool,
) -> Result<CommandResult, CliError> {
    let command = capture_cwd(command);
    let envelope = CommandEnvelope::new(CommandId::new(), source, SystemTime::now(), command);
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
    };
    match exchange(endpoint, session_id, request, names_client)? {
        IpcResult::CommandResult(result) => Ok(talk::filter_rejection_hint(result)),
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(talk::SESSION.unexpected_reply(&other)),
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
///
/// The answer passes through
/// [`filter_reported_text`](crate::discovery::filter_reported_text) before it
/// is handed back, so every name, title, working directory and argv in it is
/// filtered whichever session answered.
fn overview_of(
    endpoint: &EndpointFile,
    session_id: SessionId,
) -> Result<SessionOverview, CliError> {
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Discovery,
    };
    match exchange(endpoint, session_id, request, false)? {
        IpcResult::Overview(mut overview) => {
            crate::discovery::filter_reported_text(&mut overview);
            Ok(overview)
        }
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(talk::SESSION.unexpected_reply(&other)),
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
/// session from this build or newer names the kind it lacks
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
    match exchange(&endpoint, session_id, request, false)? {
        IpcResult::Layout(layout) => match tab {
            Some(tab_id) if layout.tabs.is_empty() => Err(CliError::CommandRejected {
                reason: RejectReason::TargetNotFound,
                help: Some(format!("no running session has tab {tab_id}")),
            }),
            _ => Ok(layout),
        },
        IpcResult::Error(refusal) if session_has_no_such_request(refusal.code) => {
            Err(CliError::IpcUnavailable {
                detail: "this session was started by an older koshi that cannot report its \
                         layout; restart the session to use `debug dump-layout`, or run \
                         `koshi debug dump-state`, which this session does answer"
                    .to_string(),
            })
        }
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(talk::SESSION.unexpected_reply(&other)),
    }
}

/// Ask `session_id` for the events it published most recently, oldest first.
///
/// A session with no such request kind answers `UnsupportedKind`, and a
/// session that cannot read the bytes answers `MalformedRequest`; both become
/// a [`CliError::IpcUnavailable`] naming what to do instead. Every other
/// refusal carries its own message through.
pub fn fetch_recent_events(
    runtime_dir: &Path,
    session_id: SessionId,
) -> Result<Vec<RecentEvent>, CliError> {
    let endpoint = read_endpoint(runtime_dir, session_id)?;
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::RecentEvents,
    };
    match exchange(&endpoint, session_id, request, false)? {
        IpcResult::RecentEvents(events) => Ok(events),
        IpcResult::Error(refusal) if session_has_no_such_request(refusal.code) => {
            Err(CliError::IpcUnavailable {
                detail: "this session was started by an older koshi that keeps no recent-events \
                         buffer; restart the session to use `debug events`"
                    .to_string(),
            })
        }
        IpcResult::Error(refusal) => Err(refused(&refusal)),
        other => Err(talk::SESSION.unexpected_reply(&other)),
    }
}

/// True when `code` says the session's build has no such request kind: the
/// session named the kind as one it lacks
/// ([`UnsupportedKind`](IpcErrorCode::UnsupportedKind)), or it could not read
/// the request's bytes at all
/// ([`MalformedRequest`](IpcErrorCode::MalformedRequest)).
fn session_has_no_such_request(code: IpcErrorCode) -> bool {
    matches!(
        code,
        IpcErrorCode::UnsupportedKind | IpcErrorCode::MalformedRequest
    )
}

/// The result of asking a running session to restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRestart {
    /// The session answered that it is replacing its own process image.
    Restarting,
    /// Nothing advertises that session, or nothing listens behind the address
    /// it advertises, so nothing restarted.
    NotRunning,
    /// The session runs a koshi build that has no restart request: it named
    /// the kind it lacks, or it could not read the request's bytes at all.
    TooOld,
}

/// Ask the running session `session_id` to restart into the binary on disk.
///
/// Sends exactly one Restart exchange and never starts a session. Every pane,
/// its child process, its terminal and its scrollback stay as they are, so a
/// client that was attached attaches again and finds the session it left.
///
/// A session that refuses the request gives [`CliError::IpcUnavailable`]
/// carrying the sentence the session sent. A session whose build has no such
/// request reads as [`SessionRestart::TooOld`]: one from this build or newer
/// names the kind it lacks ([`UnsupportedKind`](IpcErrorCode::UnsupportedKind)),
/// and one older than the tolerant wire cannot read the request at all
/// ([`MalformedRequest`](IpcErrorCode::MalformedRequest)).
pub fn restart_running_session(
    runtime_dir: &Path,
    session_id: SessionId,
) -> Result<SessionRestart, CliError> {
    let endpoint = match read_endpoint(runtime_dir, session_id) {
        Ok(endpoint) => endpoint,
        Err(CliError::SessionNotFound { .. }) => return Ok(SessionRestart::NotRunning),
        Err(other) => return Err(other),
    };
    let request = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Restart,
    };
    match exchange(&endpoint, session_id, request, false) {
        Ok(IpcResult::Restarting) => Ok(SessionRestart::Restarting),
        Ok(IpcResult::Error(refusal)) if session_has_no_such_request(refusal.code) => {
            Ok(SessionRestart::TooOld)
        }
        Ok(IpcResult::Error(refusal)) => Err(refused(&refusal)),
        Ok(other) => Err(talk::SESSION.unexpected_reply(&other)),
        Err(CliError::SessionNotFound { .. }) => Ok(SessionRestart::NotRunning),
        Err(other) => Err(other),
    }
}

/// The build version the running session `session_id` reports in its Hello
/// answer.
///
/// `Ok(None)` means no session is running under that id. An empty string means
/// the session answered but predates the version field. Sends nothing besides
/// the Hello.
pub fn running_session_version(
    runtime_dir: &Path,
    session_id: SessionId,
) -> Result<Option<String>, CliError> {
    let endpoint = match read_endpoint(runtime_dir, session_id) {
        Ok(endpoint) => endpoint,
        Err(CliError::SessionNotFound { .. }) => return Ok(None),
        Err(other) => return Err(other),
    };
    let mut connection = match connect(&endpoint, session_id) {
        Ok(connection) => connection,
        Err(CliError::SessionNotFound { .. }) => return Ok(None),
        Err(other) => return Err(other),
    };

    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::hello(endpoint.token),
    };
    connection.send(&hello).map_err(talk_failed)?;
    let reply: IncomingResponse = connection.recv().map_err(talk_failed)?;
    talk::session_hello_version(reply).map(|(_, version)| Some(version))
}

/// Every session with an endpoint file in `runtime_dir`, in no particular
/// order. A file is counted by its name alone (`session-<uuid>.json`);
/// whether anything still listens behind it is the caller's probe to make.
/// An unreadable directory reads as no sessions.
pub fn advertised_sessions(runtime_dir: &Path) -> Vec<SessionId> {
    sessions_named_by(runtime_dir, ".json")
}

/// Every session with a resume file in `runtime_dir`, in no particular order.
/// A file is counted by its name alone (`session-<uuid>` plus
/// [`RESUME_SUFFIX`]); whether that session still runs is the caller's check to
/// make. An unreadable directory reads as no sessions.
pub fn sessions_with_resume_files(runtime_dir: &Path) -> Vec<SessionId> {
    sessions_named_by(runtime_dir, RESUME_SUFFIX)
}

/// Every session `runtime_dir` holds a file for whose name is `session-<uuid>`
/// plus `suffix`, in no particular order. An unreadable directory reads as no
/// sessions.
fn sessions_named_by(runtime_dir: &Path, suffix: &str) -> Vec<SessionId> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| session_id_of(entry.file_name().to_str()?, suffix))
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
    crate::config::shared_sessions_dir(&server)
}

/// Every session another local user started that `shared_base` advertises, as
/// its id and the control-socket address reaching it, in no particular order.
///
/// An id that `runtime_dir` itself advertises is left out on both platforms:
/// this user's own session, never a foreign one standing in under the same
/// id. On Unix each user's sockets sit in a subdirectory named after that
/// user's id, and the subdirectory named after the user owning `runtime_dir`
/// is skipped as well. On Windows the markers share one flat directory.
///
/// A session is counted by its file name alone; whether anything still
/// listens behind it is the caller's probe to make. An unreadable directory
/// reads as no sessions. A `runtime_dir` that does not exist holds no
/// sessions of this user's, so no subdirectory is skipped; one whose owner
/// cannot be read reads as no sessions.
#[must_use]
pub fn foreign_sessions(shared_base: &Path, runtime_dir: &Path) -> Vec<(SessionId, String)> {
    let Ok(entries) = std::fs::read_dir(shared_base) else {
        return Vec::new();
    };
    let advertised = advertised_sessions(runtime_dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let own_subdir = match std::fs::metadata(runtime_dir) {
            Ok(dir) => Some(dir.uid().to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "the runtime directory's owner could not be read; \
                     the shared sessions are not listed"
                );
                return Vec::new();
            }
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| {
                own_subdir
                    .as_deref()
                    .is_none_or(|own| entry.file_name().to_string_lossy() != own)
            })
            .flat_map(|entry| sockets_in(&entry.path()))
            .filter(|(id, _)| !advertised.contains(id))
            .collect()
    }
    #[cfg(windows)]
    {
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let id = session_id_of(entry.file_name().to_str()?, "")?;
                (!advertised.contains(&id)).then(|| (id, shared_socket_addr(shared_base, id)))
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

/// Connect to `endpoint`, open with the Hello, and run `request` on the same
/// connection. Returns `request`'s result; a failed Hello is an error.
///
/// With `names_client` `false` the Hello and `request` go out back to back
/// before either reply is read, and the server answers every request in
/// order, so the exchange costs one round trip.
///
/// With `true` the Hello answer is read first, and a session that settled
/// below [`TARGET_CLIENT_PROTOCOL`](crate::talk::TARGET_CLIENT_PROTOCOL) is
/// refused with [`CliError::IpcUnavailable`] before `request` is written, so
/// it is never sent. That costs a second round trip.
fn exchange(
    endpoint: &EndpointFile,
    session_id: SessionId,
    request: IpcRequest,
    names_client: bool,
) -> Result<IpcResult, CliError> {
    let mut connection = connect(endpoint, session_id)?;
    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::hello(endpoint.token.clone()),
    };
    connection.send(&hello).map_err(talk_failed)?;

    if names_client {
        let hello_reply: IncomingResponse = connection.recv().map_err(talk_failed)?;
        let (settled, _) = talk::session_hello_version(hello_reply)?;
        talk::require_client_targeting(settled, true)?;
        connection.send(&request).map_err(talk_failed)?;
    } else {
        connection.send(&request).map_err(talk_failed)?;
        let hello_reply: IncomingResponse = connection.recv().map_err(talk_failed)?;
        talk::session_hello_version(hello_reply)?;
    }

    let reply: IncomingResponse = connection.recv().map_err(talk_failed)?;
    talk::SESSION.take_result(reply)
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
pub fn read_endpoint(runtime_dir: &Path, session_id: SessionId) -> Result<EndpointFile, CliError> {
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

/// Connect to the advertised socket. An address nothing listens on reports the
/// session as not running ([`CliError::SessionNotFound`]); every other
/// transport failure is [`CliError::IpcUnavailable`].
pub fn connect(endpoint: &EndpointFile, session_id: SessionId) -> Result<Connection, CliError> {
    Connection::connect(&endpoint.socket).map_err(|error| match error {
        IpcError::NoListener { .. } => CliError::SessionNotFound {
            session: session_id.to_string(),
        },
        other => CliError::IpcUnavailable {
            detail: other.to_string(),
        },
    })
}

#[cfg(test)]
mod tests;
