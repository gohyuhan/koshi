//! The client side of the router socket: ask, and start the router first when
//! none is running.
//!
//! The router's endpoint file in the private runtime directory advertises its
//! socket address and connection token; reading it is the same-user proof the
//! Hello presents. The Hello and the request are written back to back before
//! either reply is read, so an exchange costs one round trip.
//!
//! No endpoint file, or nothing listening at the address one names, means no
//! router is running. Then this starts one detached and retries the exchange
//! until the new router answers or the wait runs out — the same path a request
//! takes when it arrives just as an idle router exits.
//!
//! Three asks never start one: restarting the running router, reading its
//! build version, and counting the connections it holds from another machine.
//! Each opens one connection, and reports back when no router was running.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use koshi_core::ids::SessionId;
use koshi_core::text::sanitize_reported_text;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    resolve_router_endpoint_path, IncomingRouterResponse, RouterRequest, RouterRequestKind,
    RouterResult,
};
use koshi_ipc::transport::Connection;

use crate::error::CliError;
use crate::talk::{self, build_ipc_unavailable_error};

#[cfg(test)]
mod tests;

/// The subcommand this binary starts itself under to run the router. The
/// arguments after it are [`RUNTIME_DIRECTORY_FLAG`] with the directory to serve,
/// and `--wait-for-lock` when a router hands its place to a replacement.
pub const ROUTER_SUBCOMMAND: &str = "serve-router";

/// The flag naming the runtime directory a started process serves. Takes that
/// directory as its value.
pub const RUNTIME_DIRECTORY_FLAG: &str = "--runtime-dir";

/// How long a freshly started router has to bind its socket and advertise it
/// before the request gives up.
const ROUTER_START_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// How long the retry loop pauses between connect attempts while it waits for
/// a freshly started router.
const ROUTER_START_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(100);

/// The Win32 `DETACHED_PROCESS` creation flag: the started process gets no
/// console and does not inherit the caller's.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// The Win32 `CREATE_NEW_PROCESS_GROUP` creation flag: the started process
/// begins a process group of its own.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Ask the router for `request_kind` and hand back its answer.
///
/// Tries the exchange once. With no router running it starts one detached and
/// retries every 100 milliseconds until the router answers or 5 seconds pass.
/// Nothing is sent on an attempt that finds no router: a retried request
/// reaches a router exactly once.
///
/// The answer is the router's own result for `request_kind`, including a
/// [`RouterResult::Error`] refusing it. A refused Hello, a reply answering
/// nothing that was asked, and every failure to talk are
/// [`CliError::IpcUnavailable`].
pub fn submit_router_request(
    runtime_directory: &Path,
    request_kind: RouterRequestKind,
) -> Result<RouterResult, CliError> {
    if let Some(router_result) = exchange_router_request(runtime_directory, &request_kind)? {
        return Ok(router_result);
    }
    spawn_router_detached(runtime_directory)?;

    let deadline = Instant::now() + ROUTER_START_TIMEOUT_DURATION;
    loop {
        if let Some(router_result) = exchange_router_request(runtime_directory, &request_kind)? {
            return Ok(router_result);
        }
        if Instant::now() >= deadline {
            return Err(CliError::IpcUnavailable {
                detail: "the router did not start".to_string(),
            });
        }
        std::thread::sleep(ROUTER_START_POLL_INTERVAL_DURATION);
    }
}

/// Ask the router that is already running to restart into the binary on disk.
///
/// Sends exactly one Restart exchange and never starts a router. `Ok(false)`
/// means no router was running, so nothing restarted.
///
/// A router that refuses the request is [`CliError::IpcUnavailable`] carrying
/// the sentence the router sent, filtered by [`sanitize_reported_text`]. A
/// router whose build has no Restart kind refuses it, and that sentence names
/// both builds.
pub fn restart_running_router(runtime_directory: &Path) -> Result<bool, CliError> {
    match exchange_router_request(runtime_directory, &RouterRequestKind::Restart)? {
        None => Ok(false),
        Some(RouterResult::Restarting) => Ok(true),
        Some(RouterResult::Error(refusal)) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        Some(unexpected_router_result) => {
            Err(talk::ROUTER_PEER_WORDS.build_unexpected_reply_error(&unexpected_router_result))
        }
    }
}

/// What asking the running router for its count of connections from another
/// machine produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteConnections {
    /// A router answered. `Some(n)` is the count of connections it holds from
    /// another machine, whether they have attached to a session or not.
    /// `Some(0)` is a router holding none; `None` is a router whose build
    /// reports no count at all.
    Answered(Option<usize>),
    /// No endpoint file, or nothing listening behind it.
    NotRunning,
    /// A router is listening and has no request kind by this name.
    OlderBuild,
    /// A router is listening and did not answer the question.
    NoAnswer {
        /// Why there is no count: the sentence the router refused with, the
        /// sentence naming a reply that answers nothing this asked, or the
        /// sentence naming a failure to talk.
        error_detail: String,
    },
}

/// How many connections from another machine the running router holds
/// admitted, whether they have attached to a session or not.
///
/// Sends exactly one RemoteStatus exchange and never starts a router.
///
/// A router whose build has no such request kind refuses it with
/// [`IpcErrorCode::UnsupportedKind`], which is
/// [`RemoteConnections::OlderBuild`]. Every other refusal, unexpected reply
/// and transport failure is [`RemoteConnections::NoAnswer`].
#[must_use]
pub fn query_running_router_remote_connections(runtime_directory: &Path) -> RemoteConnections {
    match exchange_router_request(runtime_directory, &RouterRequestKind::RemoteStatus) {
        Ok(None) => RemoteConnections::NotRunning,
        Ok(Some(RouterResult::RemoteStatus {
            remote_connection_count,
            ..
        })) => RemoteConnections::Answered(remote_connection_count),
        Ok(Some(RouterResult::Error(refusal))) if refusal.code == IpcErrorCode::UnsupportedKind => {
            RemoteConnections::OlderBuild
        }
        Ok(Some(RouterResult::Error(refusal))) => RemoteConnections::NoAnswer {
            error_detail: refusal.message,
        },
        Ok(Some(unexpected_router_result)) => RemoteConnections::NoAnswer {
            error_detail: talk::ROUTER_PEER_WORDS
                .build_unexpected_reply_error(&unexpected_router_result)
                .to_string(),
        },
        Err(ipc_error) => RemoteConnections::NoAnswer {
            error_detail: ipc_error.to_string(),
        },
    }
}

/// The build version the running router reports in its Hello answer.
///
/// `Ok(None)` means no router is running. An empty string means the router
/// answered but predates the version field. Sends nothing besides the Hello;
/// never starts a router.
pub fn get_running_router_version(runtime_directory: &Path) -> Result<Option<String>, CliError> {
    let Some((mut connection, endpoint)) = connect_to_running_router(runtime_directory)? else {
        return Ok(None);
    };
    let hello_request = RouterRequest {
        request_id: 1,
        request_kind: RouterRequestKind::build_hello_request(endpoint.connection_token),
    };
    connection
        .send(&hello_request)
        .map_err(build_ipc_unavailable_error)?;
    let hello_response: IncomingRouterResponse =
        connection.recv().map_err(build_ipc_unavailable_error)?;
    talk::parse_router_hello_version(hello_response).map(Some)
}

/// A connection to the running router, with the endpoint file that named it.
///
/// `Ok(None)` means no router is running — the endpoint file is missing, or
/// nothing listens at the address it names. Nothing is sent.
fn connect_to_running_router(
    runtime_directory: &Path,
) -> Result<Option<(Connection, EndpointFile)>, CliError> {
    let endpoint =
        match EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory)) {
            Ok(endpoint) => endpoint,
            Err(IpcError::EndpointFileMissing { .. }) => return Ok(None),
            Err(ipc_error) => return Err(build_ipc_unavailable_error(ipc_error)),
        };
    match Connection::connect(&endpoint.socket_address) {
        Ok(connection) => Ok(Some((connection, endpoint))),
        Err(IpcError::NoListener { .. }) => Ok(None),
        Err(ipc_error) => Err(build_ipc_unavailable_error(ipc_error)),
    }
}

/// One exchange with a running router: read its endpoint file, connect,
/// pipeline the Hello and `request_kind` back to back, and read both replies in order.
///
/// A [`RouterResult::Error`] comes back through
/// [`rewrite_router_result_for_build`], so its
/// message is filtered before any caller reports it.
///
/// `Ok(None)` means no router is running — the endpoint file is missing, or
/// nothing listens at the address it names — and nothing was sent.
fn exchange_router_request(
    runtime_directory: &Path,
    request_kind: &RouterRequestKind,
) -> Result<Option<RouterResult>, CliError> {
    let Some((mut connection, endpoint)) = connect_to_running_router(runtime_directory)? else {
        return Ok(None);
    };
    let hello_request = RouterRequest {
        request_id: 1,
        request_kind: RouterRequestKind::build_hello_request(endpoint.connection_token),
    };
    let router_request = RouterRequest {
        request_id: 2,
        request_kind: request_kind.clone(),
    };
    connection
        .send(&hello_request)
        .map_err(build_ipc_unavailable_error)?;
    connection
        .send(&router_request)
        .map_err(build_ipc_unavailable_error)?;

    let hello_response: IncomingRouterResponse =
        connection.recv().map_err(build_ipc_unavailable_error)?;
    let router_version = talk::parse_router_hello_version(hello_response)?;

    let router_response: IncomingRouterResponse =
        connection.recv().map_err(build_ipc_unavailable_error)?;
    talk::ROUTER_PEER_WORDS
        .take_response_result(router_response)
        .map(|router_result| {
            Some(rewrite_router_result_for_build(
                router_result,
                &router_version,
            ))
        })
}

/// A refusal filtered by [`sanitize_reported_text`], and — for a request kind
/// the router does not have — restated to name both builds.
///
/// Every refusal passes through this, so the sentence a caller reports carries
/// no control, bidi-control or tag character, and no more than
/// [`MAX_REPORTED_TEXT_BYTE_COUNT`](koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT) of
/// what the router sent.
///
/// Only [`IpcErrorCode::UnsupportedKind`] is restated, and only when the
/// router reports a build other than this one. Every answer that is not a
/// refusal passes through unchanged.
///
/// `router_version` is the build the router reported in its Hello, empty when
/// the router predates that field.
fn rewrite_router_result_for_build(
    router_result: RouterResult,
    router_version: &str,
) -> RouterResult {
    let current_build_version = env!("CARGO_PKG_VERSION");
    let RouterResult::Error(refusal) = router_result else {
        return router_result;
    };
    let sanitized_refusal_message = sanitize_reported_text(&refusal.message);
    if refusal.code != IpcErrorCode::UnsupportedKind || router_version == current_build_version {
        return RouterResult::Error(IpcErrorPayload {
            code: refusal.code,
            message: sanitized_refusal_message,
        });
    }
    let running_router_description = if router_version.is_empty() {
        "an older koshi that does not report its build".to_string()
    } else {
        format!("koshi {router_version}")
    };
    RouterResult::Error(IpcErrorPayload {
        code: refusal.code,
        message: format!(
            "{sanitized_refusal_message} — the running router is {running_router_description} \
             and this command is koshi {current_build_version}; the router serves its own build \
             until it restarts, which it does \
             once no session is left running"
        ),
    })
}

/// Start the router as a detached process serving `runtime_directory`.
///
/// It gets no standard input, output, or error, and a process group of its
/// own: it keeps running after the shell that started it goes away, and writes
/// nothing over the caller's terminal.
fn spawn_router_detached(runtime_directory: &Path) -> Result<(), CliError> {
    let current_executable =
        std::env::current_exe().map_err(|io_error| CliError::IpcUnavailable {
            detail: format!("this binary's own path could not be read: {io_error}"),
        })?;
    let mut router_process_command = std::process::Command::new(current_executable);
    router_process_command
        .arg(ROUTER_SUBCOMMAND)
        .arg(RUNTIME_DIRECTORY_FLAG)
        .arg(runtime_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        router_process_command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        router_process_command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    // The child handle is dropped. On Unix, a router that exits while this
    // process remains alive stays a zombie until this process exits.
    router_process_command
        .spawn()
        .map(|_| ())
        .map_err(|io_error| CliError::IpcUnavailable {
            detail: format!("the router could not be started: {io_error}"),
        })
}

/// Ask the router to make a new session and hand back its id. Starts a router
/// first when none is running.
///
/// The session's first shell opens in the directory this command was run in.
/// A directory that cannot be read is sent as `None`, and the session server
/// keeps the directory it inherited.
///
/// `is_other_user_access_allowed` `Some(true)` lets the other users of this machine reach
/// the new session whatever its `koshi.kdl` says; `None` leaves that answer to
/// the file.
///
/// # Errors
/// Returns [`CliError::IpcUnavailable`] when the router refuses the create or
/// answers with anything other than the new session.
pub fn request_new_session(
    runtime_directory: &Path,
    profile_name: Option<&str>,
    is_other_user_access_allowed: Option<bool>,
) -> Result<SessionId, CliError> {
    let create_session_request_kind = RouterRequestKind::CreateSession {
        profile: profile_name.map(str::to_string),
        working_directory: std::env::current_dir().ok(),
        is_other_user_access_allowed,
    };
    match submit_router_request(runtime_directory, create_session_request_kind)? {
        RouterResult::Created(session_address) => Ok(session_address.session_id),
        RouterResult::Error(refusal) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        unexpected_result => {
            Err(talk::ROUTER_PEER_WORDS.build_unexpected_reply_error(&unexpected_result))
        }
    }
}
