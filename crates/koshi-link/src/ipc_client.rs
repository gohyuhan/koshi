//! The CLI side of the control socket: find the session's endpoint, connect,
//! open with Hello, submit one command, and read back its result.
//!
//! The endpoint file in the private runtime directory advertises the
//! session's socket address and connection token; reading it is the
//! same-user proof the Hello presents. The Hello and the command are written
//! back to back before either reply is read, so a submission costs one round
//! trip, whether or not it names a target client. A session that settles on a
//! version this build does not speak reads both, and the caller is refused on
//! the Hello answer and never given a command result.
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
use koshi_ipc::endpoint::{compute_shared_socket_address, EndpointFile, RESUME_SUFFIX};
use koshi_ipc::error::IpcError;
use koshi_ipc::layout::SessionLayout;
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcErrorCode, IpcRequest, IpcRequestKind, IpcResult,
};
use koshi_ipc::transport::Connection;
use uuid::Uuid;

use crate::error::CliError;
use crate::in_session::InSessionContext;
use crate::talk::{self, build_ipc_unavailable_error, build_peer_refusal_error};

/// The private runtime directory holding every endpoint file, or
/// [`CliError::IpcUnavailable`] when the machine has none.
pub fn resolve_runtime_directory() -> Result<PathBuf, CliError> {
    koshi_paths::resolve_runtime_directory().ok_or_else(|| CliError::IpcUnavailable {
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
pub fn submit_in_session_command(
    in_session_context: &InSessionContext,
    command: Command,
) -> Result<CommandResult, CliError> {
    submit_command_via_runtime_directory(&resolve_runtime_directory()?, in_session_context, command)
}

/// Submit `command` to the running session `session_id` as an external
/// invocation — a `koshi` command typed outside any pane, or inside a pane
/// but targeting another session. Same exchange and error mapping as
/// [`submit_in_session_command`]; the envelope's source is
/// [`CommandSource::ExternalCli`], so the runtime resolves defaults through
/// the target session's acting client rather than an issuing pane.
///
/// `client_id` is the client the command acts for, and rides on the source.
/// Naming one changes nothing about the exchange: the Hello and the command go
/// out back to back, and it costs one round trip either way.
pub fn submit_external_command(
    session_id: SessionId,
    client_id: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    submit_external_command_via_runtime_directory(
        &resolve_runtime_directory()?,
        session_id,
        client_id,
        command,
    )
}

/// [`submit_in_session_command`] against an explicit runtime directory: the whole
/// exchange, with the endpoint lookup rooted where the caller says.
fn submit_command_via_runtime_directory(
    runtime_directory: &Path,
    in_session_context: &InSessionContext,
    command: Command,
) -> Result<CommandResult, CliError> {
    let endpoint_file = load_session_endpoint(runtime_directory, in_session_context.session_id)?;
    let command_source = CommandSource::from_in_session_cli(
        in_session_context.session_id,
        in_session_context.client_id,
        in_session_context.pane_id,
        PathBuf::from(&endpoint_file.socket_address),
    );
    submit_command_envelope(
        &endpoint_file,
        in_session_context.session_id,
        command_source,
        command,
    )
}

/// [`submit_external_command`] against an explicit runtime directory: the whole
/// exchange, with the endpoint lookup rooted where the caller says.
/// `client_id` rides on the source.
pub fn submit_external_command_via_runtime_directory(
    runtime_directory: &Path,
    session_id: SessionId,
    client_id: Option<ClientId>,
    command: Command,
) -> Result<CommandResult, CliError> {
    let endpoint_file = load_session_endpoint(runtime_directory, session_id)?;
    let command_source = CommandSource::from_external_cli(Some(session_id), client_id);
    submit_command_envelope(&endpoint_file, session_id, command_source, command)
}

/// Fill a pane-creating command's unset working directory with this CLI
/// process's own, read here at send time, so the new pane opens where the
/// command was run. A command that already names a directory is left alone,
/// and every other command carries none.
fn capture_current_working_directory(mut command: Command) -> Command {
    let working_directory = match &mut command {
        Command::NewPane(command_args) => &mut command_args.working_directory,
        Command::NewTab(command_args) => &mut command_args.working_directory,
        Command::RunCommandPane(command_args) => &mut command_args.working_directory,
        _ => return command,
    };
    if working_directory.is_none() {
        *working_directory = std::env::current_dir().ok();
    }
    command
}

/// One command submission over `endpoint`: connect, send the Hello and the
/// enveloped command, and read the command's result. A pane-creating command
/// with no directory of its own gets this process's current directory, and a
/// rejection's hint is filtered by
/// [`filter_rejection_hint`](crate::talk::filter_rejection_hint).
fn submit_command_envelope(
    endpoint: &EndpointFile,
    session_id: SessionId,
    command_source: CommandSource,
    command: Command,
) -> Result<CommandResult, CliError> {
    let prepared_command = capture_current_working_directory(command);
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        command_source,
        SystemTime::now(),
        prepared_command,
    );
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::SubmitCommand(Box::new(command_envelope)),
    };
    match exchange_session_request(endpoint, session_id, ipc_request)? {
        IpcResult::CommandResult(command_result) => Ok(talk::filter_rejection_hint(command_result)),
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// Ask the running session `session_id` to describe itself in full: tabs,
/// panes, and attached clients ([`SessionOverview`]). The routing layer uses
/// the answer to resolve names to ids and to find which session owns an
/// explicitly named pane, tab, or client.
pub fn fetch_session_overview(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Result<SessionOverview, CliError> {
    describe_session(
        &load_session_endpoint(runtime_directory, session_id)?,
        session_id,
    )
}

/// Ask the session `session_id` listening at `socket` to describe itself, as
/// a session another local user started: the address is the one the shared
/// directory advertised, and the token presented is empty.
pub fn fetch_foreign_session_overview(
    session_id: SessionId,
    socket_address: &str,
) -> Result<SessionOverview, CliError> {
    describe_session(
        &build_foreign_session_endpoint(socket_address.to_string()),
        session_id,
    )
}

/// One Discovery exchange over `endpoint`, for the session `session_id`.
///
/// The answer passes through
/// [`filter_session_overview_text`](crate::discovery::filter_session_overview_text) before it
/// is handed back, so every name, title, working directory and argv in it is
/// filtered whichever session answered.
fn describe_session(
    endpoint: &EndpointFile,
    session_id: SessionId,
) -> Result<SessionOverview, CliError> {
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Discovery,
    };
    match exchange_session_request(endpoint, session_id, ipc_request)? {
        IpcResult::Overview(mut session_overview) => {
            crate::discovery::filter_session_overview_text(&mut session_overview);
            Ok(session_overview)
        }
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
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
    runtime_directory: &Path,
    session_id: SessionId,
    tab_id: Option<TabId>,
) -> Result<SessionLayout, CliError> {
    let session_endpoint = load_session_endpoint(runtime_directory, session_id)?;
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Layout { tab_id },
    };
    match exchange_session_request(&session_endpoint, session_id, ipc_request)? {
        IpcResult::Layout(session_layout) => match tab_id {
            Some(tab_id) if session_layout.tabs.is_empty() => Err(CliError::CommandRejected {
                reason: RejectReason::TargetNotFound,
                help: Some(format!("no running session has tab {tab_id}")),
            }),
            _ => Ok(session_layout),
        },
        IpcResult::Error(refusal) if is_session_request_unavailable(refusal.code) => {
            Err(CliError::IpcUnavailable {
                detail: "this session was started by an older koshi that cannot report its \
                         layout; restart the session to use `debug dump-layout`, or run \
                         `koshi debug dump-state`, which this session does answer"
                    .to_string(),
            })
        }
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// Ask `session_id` for the events it published most recently, oldest first.
///
/// A session with no such request kind answers `UnsupportedKind`, and a
/// session that cannot read the bytes answers `MalformedRequest`; both become
/// a [`CliError::IpcUnavailable`] naming what to do instead. Every other
/// refusal carries its own message through.
pub fn fetch_recent_events(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Result<Vec<RecentEvent>, CliError> {
    let session_endpoint = load_session_endpoint(runtime_directory, session_id)?;
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::RecentEvents,
    };
    match exchange_session_request(&session_endpoint, session_id, ipc_request)? {
        IpcResult::RecentEvents(recent_events) => Ok(recent_events),
        IpcResult::Error(refusal) if is_session_request_unavailable(refusal.code) => {
            Err(CliError::IpcUnavailable {
                detail: "this session was started by an older koshi that keeps no recent-events \
                         buffer; restart the session to use `debug events`"
                    .to_string(),
            })
        }
        IpcResult::Error(refusal) => Err(build_peer_refusal_error(&refusal)),
        unexpected_result => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}

/// True when `code` says the session's build has no such request request_kind: the
/// session named the kind as one it lacks
/// ([`UnsupportedKind`](IpcErrorCode::UnsupportedKind)), or it could not read
/// the request's bytes at all
/// ([`MalformedRequest`](IpcErrorCode::MalformedRequest)).
fn is_session_request_unavailable(code: IpcErrorCode) -> bool {
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
    runtime_directory: &Path,
    session_id: SessionId,
) -> Result<SessionRestart, CliError> {
    let session_endpoint = match load_session_endpoint(runtime_directory, session_id) {
        Ok(session_endpoint) => session_endpoint,
        Err(CliError::SessionNotFound { .. }) => return Ok(SessionRestart::NotRunning),
        Err(ipc_error) => return Err(ipc_error),
    };
    let ipc_request = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Restart,
    };
    match exchange_session_request(&session_endpoint, session_id, ipc_request) {
        Ok(IpcResult::Restarting) => Ok(SessionRestart::Restarting),
        Ok(IpcResult::Error(refusal)) if is_session_request_unavailable(refusal.code) => {
            Ok(SessionRestart::TooOld)
        }
        Ok(IpcResult::Error(refusal)) => Err(build_peer_refusal_error(&refusal)),
        Ok(unexpected_result) => {
            Err(talk::SESSION_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
        Err(CliError::SessionNotFound { .. }) => Ok(SessionRestart::NotRunning),
        Err(ipc_error) => Err(ipc_error),
    }
}

/// The build version the running session `session_id` reports in its Hello
/// answer.
///
/// `Ok(None)` means no session is running under that id. An empty string means
/// the session answered but predates the version field. Sends nothing besides
/// the Hello.
pub fn get_running_session_version(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Result<Option<String>, CliError> {
    let session_endpoint = match load_session_endpoint(runtime_directory, session_id) {
        Ok(session_endpoint) => session_endpoint,
        Err(CliError::SessionNotFound { .. }) => return Ok(None),
        Err(ipc_error) => return Err(ipc_error),
    };
    let mut connection = match connect_to_session(&session_endpoint, session_id) {
        Ok(connection) => connection,
        Err(CliError::SessionNotFound { .. }) => return Ok(None),
        Err(ipc_error) => return Err(ipc_error),
    };

    let hello_request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::build_hello_request(session_endpoint.connection_token),
    };
    connection
        .send(&hello_request)
        .map_err(build_ipc_unavailable_error)?;
    let hello_response: IncomingResponse =
        connection.recv().map_err(build_ipc_unavailable_error)?;
    talk::parse_session_hello_version(hello_response).map(|(_, version)| Some(version))
}

/// Every session with an endpoint file in `runtime_directory`, in no particular
/// order. A file is counted by its name alone (`session-<uuid>.json`);
/// whether anything still listens behind it is the caller's probe to make.
/// An unreadable directory reads as no sessions.
pub fn list_advertised_sessions(runtime_directory: &Path) -> Vec<SessionId> {
    list_session_ids_named_by_suffix(runtime_directory, ".json")
}

/// Every session with a resume file in `runtime_directory`, in no particular order.
/// A file is counted by its name alone (`session-<uuid>` plus
/// [`RESUME_SUFFIX`]); whether that session still runs is the caller's check to
/// make. An unreadable directory reads as no sessions.
pub fn list_sessions_with_resume_files(runtime_directory: &Path) -> Vec<SessionId> {
    list_session_ids_named_by_suffix(runtime_directory, RESUME_SUFFIX)
}

/// Every session `runtime_directory` holds a file for whose name is `session-<uuid>`
/// plus `file_name_suffix`, in no particular order. An unreadable directory reads as no
/// sessions.
fn list_session_ids_named_by_suffix(
    runtime_directory: &Path,
    file_name_suffix: &str,
) -> Vec<SessionId> {
    let Ok(directory_entries) = std::fs::read_dir(runtime_directory) else {
        return Vec::new();
    };
    directory_entries
        .filter_map(Result::ok)
        .filter_map(|directory_entry| {
            parse_session_id(directory_entry.file_name().to_str()?, file_name_suffix)
        })
        .collect()
}

/// The session a file named `session-<uuid>` plus `suffix` names. Any other
/// name is `None`.
fn parse_session_id(file_name: &str, file_name_suffix: &str) -> Option<SessionId> {
    let session_uuid_text = file_name
        .strip_suffix(file_name_suffix)?
        .strip_prefix("session-")?;
    Some(SessionId::from_uuid(
        Uuid::parse_str(session_uuid_text).ok()?,
    ))
}

/// The machine-wide directory holding the sessions other local users started,
/// or `None` while `allow-other-users` is off in `koshi.kdl` or the machine
/// reports no such directory.
///
/// `koshi.kdl` is read again on each call, so the answer is the one the file
/// holds at this moment.
#[must_use]
pub fn resolve_shared_sessions_base_directory() -> Option<PathBuf> {
    let server_config = crate::config::load_current_server_config();
    if !server_config.should_allow_other_users {
        return None;
    }
    crate::config::resolve_shared_sessions_directory(&server_config)
}

/// Every session another local user started that `shared_sessions_base_directory` advertises, as
/// its id and the control-socket address reaching it, in no particular order.
///
/// A session id that `runtime_directory` itself advertises is left out on both platforms:
/// this user's own session, never a foreign one standing in under the same
/// id. On Unix each user's sockets sit in a subdirectory named after that
/// user's id, and the subdirectory named after the user owning `runtime_directory`
/// is skipped as well. On Windows the markers share one flat directory.
///
/// A session is counted by its file name alone; whether anything still
/// listens behind it is the caller's probe to make. An unreadable directory
/// reads as no sessions. A `runtime_directory` that does not exist holds no
/// sessions of this user's, so no subdirectory is skipped; one whose owner
/// cannot be read reads as no sessions.
#[must_use]
pub fn list_foreign_sessions(
    shared_sessions_base_directory: &Path,
    runtime_directory: &Path,
) -> Vec<(SessionId, String)> {
    let Ok(directory_entries) = std::fs::read_dir(shared_sessions_base_directory) else {
        return Vec::new();
    };
    let advertised_session_ids = list_advertised_sessions(runtime_directory);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let own_user_directory_name = match std::fs::metadata(runtime_directory) {
            Ok(directory_metadata) => Some(directory_metadata.uid().to_string()),
            Err(io_error) if io_error.kind() == std::io::ErrorKind::NotFound => None,
            Err(io_error) => {
                tracing::warn!(
                    %io_error,
                    "the runtime directory's owner could not be read; \
                     the shared sessions are not listed"
                );
                return Vec::new();
            }
        };
        directory_entries
            .filter_map(Result::ok)
            .filter(|directory_entry| {
                own_user_directory_name
                    .as_deref()
                    .is_none_or(|own_user_directory_name| {
                        directory_entry.file_name().to_string_lossy() != own_user_directory_name
                    })
            })
            .flat_map(|directory_entry| list_user_session_sockets(&directory_entry.path()))
            .filter(|(session_id, _)| !advertised_session_ids.contains(session_id))
            .collect()
    }
    #[cfg(windows)]
    {
        directory_entries
            .filter_map(Result::ok)
            .filter_map(|directory_entry| {
                let session_id = parse_session_id(directory_entry.file_name().to_str()?, "")?;
                (!advertised_session_ids.contains(&session_id)).then(|| {
                    (
                        session_id,
                        compute_shared_socket_address(shared_sessions_base_directory, session_id),
                    )
                })
            })
            .collect()
    }
}

/// The sessions one user's subdirectory of the shared directory advertises,
/// each as its id and the socket file reaching it. An unreadable
/// subdirectory reads as no sessions.
#[cfg(unix)]
fn list_user_session_sockets(user_directory: &Path) -> Vec<(SessionId, String)> {
    let Ok(directory_entries) = std::fs::read_dir(user_directory) else {
        return Vec::new();
    };
    directory_entries
        .filter_map(Result::ok)
        .filter_map(|directory_entry| {
            let session_id = parse_session_id(directory_entry.file_name().to_str()?, ".sock")?;
            Some((
                session_id,
                compute_shared_socket_address(user_directory, session_id),
            ))
        })
        .collect()
}

/// How to reach the session another local user started at `socket`: the
/// address the shared directory advertised, and the empty token that session
/// asks another user for. That user's own endpoint file stays unread.
fn build_foreign_session_endpoint(socket_address: String) -> EndpointFile {
    EndpointFile {
        socket_address,
        connection_token: ConnectionToken::from_secret(""),
        process_id: 0,
    }
}

/// Connect to `endpoint`, open with the Hello, and run `ipc_request` on the same
/// connection. Returns `ipc_request`'s result; a failed Hello is an error.
///
/// The Hello and `ipc_request` go out back to back before either reply is read,
/// and the server answers every request in order, so the exchange costs one
/// round trip.
fn exchange_session_request(
    endpoint: &EndpointFile,
    session_id: SessionId,
    ipc_request: IpcRequest,
) -> Result<IpcResult, CliError> {
    let mut connection = connect_to_session(endpoint, session_id)?;
    let hello_request = IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::build_hello_request(endpoint.connection_token.clone()),
    };
    connection
        .send(&hello_request)
        .map_err(build_ipc_unavailable_error)?;

    connection
        .send(&ipc_request)
        .map_err(build_ipc_unavailable_error)?;
    let hello_response: IncomingResponse =
        connection.recv().map_err(build_ipc_unavailable_error)?;
    talk::parse_session_hello_version(hello_response)?;

    let ipc_response: IncomingResponse = connection.recv().map_err(build_ipc_unavailable_error)?;
    talk::SESSION_PEER_WORDS.take_response_result(ipc_response)
}

/// How to reach `session_id`: the endpoint file in `runtime_directory`, or — for a
/// session another local user started — what the shared directory advertises
/// for it.
///
/// A session another local user started writes its endpoint file into that
/// user's own runtime directory, which this user may not read and does not
/// need: the shared directory names the socket, and the token presented is
/// empty. An id neither place holds means no running koshi advertises that
/// session.
pub fn load_session_endpoint(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Result<EndpointFile, CliError> {
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    match EndpointFile::load_from_path(&endpoint_path) {
        Ok(endpoint) => Ok(endpoint),
        Err(IpcError::EndpointFileMissing { .. }) => resolve_shared_sessions_base_directory()
            .into_iter()
            .flat_map(|shared_base_directory| {
                list_foreign_sessions(&shared_base_directory, runtime_directory)
            })
            .find(|(found_session_id, _)| *found_session_id == session_id)
            .map(|(_, socket_address)| build_foreign_session_endpoint(socket_address))
            .ok_or_else(|| CliError::SessionNotFound {
                session_name: session_id.to_string(),
            }),
        Err(ipc_error) => Err(CliError::IpcUnavailable {
            detail: ipc_error.to_string(),
        }),
    }
}

/// Connect to the advertised socket. An address nothing listens on reports the
/// session as not running ([`CliError::SessionNotFound`]); every other
/// transport failure is [`CliError::IpcUnavailable`].
pub fn connect_to_session(
    endpoint: &EndpointFile,
    session_id: SessionId,
) -> Result<Connection, CliError> {
    Connection::connect(&endpoint.socket_address).map_err(|ipc_error| match ipc_error {
        IpcError::NoListener { .. } => CliError::SessionNotFound {
            session_name: session_id.to_string(),
        },
        ipc_error => CliError::IpcUnavailable {
            detail: ipc_error.to_string(),
        },
    })
}

#[cfg(test)]
mod tests;
