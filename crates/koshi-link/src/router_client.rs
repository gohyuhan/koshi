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
use std::time::{Duration, Instant};

use koshi_core::ids::SessionId;
use koshi_core::text::sanitize_reported_text;
use koshi_ipc::endpoint::{is_update_restarting_servers, EndpointFile, RESTART_WINDOW_DURATION};
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    resolve_router_endpoint_path, resolve_router_program_file_path, IncomingRouterResponse,
    RouterRequest, RouterRequestKind, RouterResult, ROUTER_RESTARTING_MESSAGE,
};
use koshi_ipc::transport::Connection;

use crate::error::CliError;
use crate::ipc_client::{
    REFUSED_SERVER_RESTART_START_WAIT_DURATION, RESTART_POLL_INTERVAL_DURATION,
};
use crate::server_build::{find_refusing_server_build, RefusingServerBuild};
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

/// Ask the router for `request_kind` and hand back its answer.
///
/// Tries the exchange once. With no router running it starts one detached and
/// retries every 100 milliseconds until the router answers or 5 seconds pass.
/// Nothing is sent on an attempt that finds no router: a retried request
/// reaches a router exactly once.
///
/// A router that refuses this build's protocol version is waited for when
/// [`RefusingServerBuild::is_restart_expected`] holds for its program file.
/// The wait is [`wait_for_router_restart`], for up to
/// [`REFUSED_SERVER_RESTART_START_WAIT_DURATION`]. Once it advertises another
/// token, the exchange runs once more, as on a first try. Example: a router
/// on `0.5.0` whose program file now holds `0.6.0` refuses a `0.6.0` CLI,
/// restarts into `0.6.0`, and answers the request.
///
/// A router that refuses the request with [`ROUTER_RESTARTING_MESSAGE`] is
/// waited for the same way, for up to [`RESTART_WINDOW_DURATION`], and the
/// exchange then runs once more whatever the wait saw. Example: `koshi attach
/// work` reaches a router that has just read `0.5.1` from its program file;
/// the router refuses the lookup, restarts into `0.5.1`, and the new router
/// answers it.
///
/// The answer is the router's own result for `request_kind`, including a
/// [`RouterResult::Error`] refusing it. A Hello refused for its protocol
/// version is [`CliError::ProtocolVersionRefused`] when the router is not
/// waited for, or did not restart within the wait. Its sentence is the
/// refusal, then `; `, then what
/// [`RefusingServerBuild::format_refusal_hint`](crate::server_build::RefusingServerBuild::format_refusal_hint)
/// gives for the router's program file read at that moment, with the clause
/// `run: koshi restart-servers`. A reply in the envelope of koshi 0.4.0 or
/// older is [`CliError::PreviousReleaseServer`]. Every other refused Hello, a
/// reply answering nothing that was asked, and every other failure to talk are
/// [`CliError::IpcUnavailable`].
pub fn submit_router_request(
    runtime_directory: &Path,
    request_kind: RouterRequestKind,
) -> Result<RouterResult, CliError> {
    let first_router_result = match connect_to_running_router(runtime_directory)? {
        None => None,
        Some((connection, router_endpoint)) => {
            match exchange_router_request_on_connection(connection, &router_endpoint, &request_kind)
            {
                Err(CliError::ProtocolVersionRefused {
                    detail: refusal_detail,
                }) => {
                    let router_program_file_path =
                        resolve_router_program_file_path(runtime_directory);
                    let find_router_build = || {
                        find_refusing_server_build(
                            &router_program_file_path,
                            router_endpoint.process_id,
                        )
                    };
                    let build_refusal_error = |refusing_server_build: &RefusingServerBuild| {
                        CliError::ProtocolVersionRefused {
                            detail: format!(
                                "{refusal_detail}; {}",
                                refusing_server_build
                                    .format_refusal_hint("run: koshi restart-servers")
                            ),
                        }
                    };
                    let refusing_server_build = find_router_build();
                    if !refusing_server_build.is_restart_expected() {
                        return Err(build_refusal_error(&refusing_server_build));
                    }
                    let restart_deadline =
                        Instant::now() + REFUSED_SERVER_RESTART_START_WAIT_DURATION;
                    if !wait_for_router_restart(
                        runtime_directory,
                        &router_endpoint.connection_token,
                        restart_deadline,
                    ) {
                        return Err(build_refusal_error(&find_router_build()));
                    }
                    exchange_router_request(runtime_directory, &request_kind)?
                }
                Ok(RouterResult::Error(router_refusal))
                    if router_refusal.message == ROUTER_RESTARTING_MESSAGE =>
                {
                    let _ = wait_for_router_restart(
                        runtime_directory,
                        &router_endpoint.connection_token,
                        Instant::now() + RESTART_WINDOW_DURATION,
                    );
                    exchange_router_request(runtime_directory, &request_kind)?
                }
                exchange_result => Some(exchange_result?),
            }
        }
    };
    if let Some(router_result) = first_router_result {
        return Ok(router_result);
    }
    start_router_and_exchange(runtime_directory, || {
        exchange_router_request(runtime_directory, &request_kind)
    })
}

/// The build version a router serving `runtime_directory` reports in its Hello
/// answer. With no router running, starts one detached and asks again every
/// 100 milliseconds until it answers or 5 seconds pass.
///
/// Example: right after koshi ended a koshi 0.4.0 router, this starts a router
/// of the program this process runs, and gives `0.6.0`.
///
/// # Errors
/// What [`find_running_router_version`] gives, a router that cannot be
/// started, and [`CliError::IpcUnavailable`] reading `the router did not start`
/// when no router answers within 5 seconds.
pub fn start_router(runtime_directory: &Path) -> Result<String, CliError> {
    if let Some(router_version) = find_running_router_version(runtime_directory)? {
        return Ok(router_version);
    }
    start_router_and_exchange(runtime_directory, || {
        find_running_router_version(runtime_directory)
    })
}

/// Start the router detached, then run `router_exchange` every
/// [`ROUTER_START_POLL_INTERVAL_DURATION`] until it gives an answer, and hand
/// that answer back. `Ok(None)` from `router_exchange` means no router
/// answered yet.
///
/// # Errors
/// The failure of the start, the first failure `router_exchange` gives, and
/// [`CliError::IpcUnavailable`] reading `the router did not start` once
/// [`ROUTER_START_TIMEOUT_DURATION`] passes with no answer.
fn start_router_and_exchange<RouterAnswer>(
    runtime_directory: &Path,
    mut router_exchange: impl FnMut() -> Result<Option<RouterAnswer>, CliError>,
) -> Result<RouterAnswer, CliError> {
    spawn_router_detached(runtime_directory)?;

    let deadline = Instant::now() + ROUTER_START_TIMEOUT_DURATION;
    loop {
        if let Some(router_answer) = router_exchange()? {
            return Ok(router_answer);
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
    /// A router answered with the count of connections it holds from another
    /// machine, whether they have attached to a session or not.
    Answered(usize),
    /// No endpoint file, or nothing listening behind it.
    NotRunning,
    /// A router is listening and has no request kind by this name.
    OlderBuild,
    /// A router is listening and replies in the envelope of koshi 0.4.0 or
    /// older, which this koshi cannot talk to.
    PreviousRelease,
    /// A router is listening and did not answer the question.
    NoAnswer {
        /// Why there is no count: the sentence the router refused with, the
        /// sentence naming a reply that answers nothing this asked, or the
        /// sentence naming a failure to talk.
        error_detail: String,
        /// The process id `router.json` names, read after the failure, or
        /// `None` when that file cannot be read.
        router_process_id: Option<u32>,
    },
}

/// How many connections from another machine the running router holds
/// admitted, whether they have attached to a session or not.
///
/// Sends exactly one RemoteStatus exchange and never starts a router.
///
/// A router whose build has no such request kind refuses it with
/// [`IpcErrorCode::UnsupportedKind`], which is
/// [`RemoteConnections::OlderBuild`]. A router that replies in the envelope of
/// koshi 0.4.0 or older is [`RemoteConnections::PreviousRelease`]. Every other
/// refusal, unexpected reply and transport failure is
/// [`RemoteConnections::NoAnswer`], carrying the process id `router.json`
/// names at that moment.
#[must_use]
pub fn query_running_router_remote_connections(runtime_directory: &Path) -> RemoteConnections {
    let error_detail =
        match exchange_router_request(runtime_directory, &RouterRequestKind::RemoteStatus) {
            Ok(None) => return RemoteConnections::NotRunning,
            Ok(Some(RouterResult::RemoteStatus {
                remote_connection_count,
                ..
            })) => return RemoteConnections::Answered(remote_connection_count),
            Ok(Some(RouterResult::Error(refusal)))
                if refusal.code == IpcErrorCode::UnsupportedKind =>
            {
                return RemoteConnections::OlderBuild
            }
            Ok(Some(RouterResult::Error(refusal))) => refusal.message,
            Ok(Some(unexpected_router_result)) => talk::ROUTER_PEER_WORDS
                .build_unexpected_reply_error(&unexpected_router_result)
                .to_string(),
            Err(CliError::PreviousReleaseServer { .. }) => {
                return RemoteConnections::PreviousRelease
            }
            Err(router_exchange_error) => router_exchange_error.to_string(),
        };
    RemoteConnections::NoAnswer {
        error_detail,
        router_process_id: EndpointFile::load_from_path(&resolve_router_endpoint_path(
            runtime_directory,
        ))
        .ok()
        .map(|router_endpoint_file| router_endpoint_file.process_id),
    }
}

/// The build version the running router reports in its Hello answer.
///
/// `Ok(None)` means no router is running. Sends nothing besides the Hello;
/// never starts a router.
pub fn find_running_router_version(runtime_directory: &Path) -> Result<Option<String>, CliError> {
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
    let hello_response: IncomingRouterResponse = connection
        .recv_answer()
        .map_err(build_ipc_unavailable_error)?;
    talk::parse_router_hello_version(hello_response).map(Some)
}

/// Wait until the router's endpoint file in `runtime_directory` no longer
/// carries `router_connection_token_before_restart`, and return `true`. Return
/// `false` once `restart_deadline` has passed while no `koshi update` holds the
/// update lock in `runtime_directory` ([`is_update_restarting_servers`]).
///
/// A file with another token ends the wait, and so does a file this build
/// cannot read. A missing file and a file with the same token are read again
/// every [`RESTART_POLL_INTERVAL_DURATION`]. The first read happens before the
/// deadline is checked, so a deadline already passed still sees a router that
/// already restarted. Example: a `koshi update` that restarts sessions for 50
/// seconds before it restarts the router holds the lock all that time, and a
/// wait with a 30-second deadline ends on the router's new token.
#[must_use]
pub fn wait_for_router_restart(
    runtime_directory: &Path,
    router_connection_token_before_restart: &ConnectionToken,
    restart_deadline: Instant,
) -> bool {
    let router_endpoint_path = resolve_router_endpoint_path(runtime_directory);
    loop {
        match EndpointFile::load_from_path(&router_endpoint_path) {
            Ok(router_endpoint)
                if router_endpoint.connection_token != *router_connection_token_before_restart =>
            {
                return true;
            }
            Err(
                IpcError::EndpointFileUnreadable { .. }
                | IpcError::Koshi010WindowEndpointFile { .. },
            ) => return true,
            Ok(_) | Err(_) => {}
        }
        if Instant::now() >= restart_deadline && !is_update_restarting_servers(runtime_directory) {
            return false;
        }
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
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

/// One exchange with a running router: read its endpoint file, connect, and
/// run [`exchange_router_request_on_connection`].
///
/// `Ok(None)` means no router is running — the endpoint file is missing, or
/// nothing listens at the address it names — and nothing was sent.
fn exchange_router_request(
    runtime_directory: &Path,
    request_kind: &RouterRequestKind,
) -> Result<Option<RouterResult>, CliError> {
    let Some((connection, endpoint)) = connect_to_running_router(runtime_directory)? else {
        return Ok(None);
    };
    exchange_router_request_on_connection(connection, &endpoint, request_kind).map(Some)
}

/// Pipeline the Hello and `request_kind` back to back on `connection`, opened
/// to the router `endpoint` names, and read both replies in order.
///
/// A [`RouterResult::Error`] comes back through
/// [`rewrite_router_result_for_build`], so its
/// message is filtered before any caller reports it.
fn exchange_router_request_on_connection(
    mut connection: Connection,
    endpoint: &EndpointFile,
    request_kind: &RouterRequestKind,
) -> Result<RouterResult, CliError> {
    let hello_request = RouterRequest {
        request_id: 1,
        request_kind: RouterRequestKind::build_hello_request(endpoint.connection_token.clone()),
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

    let hello_response: IncomingRouterResponse = connection
        .recv_answer()
        .map_err(build_ipc_unavailable_error)?;
    let router_version = talk::parse_router_hello_version(hello_response)?;

    let router_response: IncomingRouterResponse = connection
        .recv_answer()
        .map_err(build_ipc_unavailable_error)?;
    talk::ROUTER_PEER_WORDS
        .take_response_result(router_response)
        .map(|router_result| rewrite_router_result_for_build(router_result, &router_version))
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
/// router reports a build other than this one: the sentence then names both
/// builds and ends `run: koshi restart-servers`. Every answer that is not a
/// refusal passes through unchanged.
///
/// `router_version` is the build the router reported in its Hello.
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
    RouterResult::Error(IpcErrorPayload {
        code: refusal.code,
        message: format!(
            "{sanitized_refusal_message} — the running router is koshi {router_version} \
             and this command is koshi {current_build_version}; run: koshi restart-servers"
        ),
    })
}

/// Start the router as a detached process serving `runtime_directory`.
///
/// It is detached as [`koshi_host::detached_process::configure_detached_process`]
/// sets it: no standard input, output, or error, and a process group of its
/// own. It keeps running after the shell that started it goes away, and writes
/// nothing over the caller's terminal.
fn spawn_router_detached(runtime_directory: &Path) -> Result<(), CliError> {
    let program_path = koshi_host::program_path::resolve_program_path().map_err(|io_error| {
        CliError::IpcUnavailable {
            detail: format!("this binary's own path could not be read: {io_error}"),
        }
    })?;
    // The child handle is dropped. On Unix, a router that exits while this
    // process remains alive stays a zombie until this process exits.
    koshi_host::detached_process::configure_detached_process(&mut std::process::Command::new(
        program_path,
    ))
    .arg(ROUTER_SUBCOMMAND)
    .arg(RUNTIME_DIRECTORY_FLAG)
    .arg(runtime_directory)
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
