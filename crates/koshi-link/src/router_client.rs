//! The client side of the router socket: ask, and start the router first when
//! none is running.
//!
//! The router's endpoint file in the private runtime directory advertises its
//! socket address and connection token; reading it is the same-user proof the
//! Hello presents. The Hello and the request are written back to back before
//! either reply is read, so an exchange costs one round trip. A Restart is
//! written once the Hello answer is read, so it costs two.
//!
//! No endpoint file, nothing listening at the address one names, and a router
//! that closes the connection before it answers the Hello all mean no router
//! is running: a router answers the Hello on a connection before it runs any
//! request read off it. Then this starts one detached and retries the
//! exchange until the new router answers or the wait runs out — the same path
//! a request takes when it arrives just as an idle router exits. When the
//! started router ends and no router answers yet, one more router is started,
//! which waits for the router lock. Example: an idle router removes its
//! endpoint file, and the router started then finds the router lock still
//! held and ends.
//!
//! An exchange that the router refuses for the connection token its endpoint
//! file carries waits until the file carries another token or cannot be read,
//! for up to 5 seconds, and runs once more.
//!
//! Three asks never start one: restarting the running router, reading its
//! build version, and reading what its remote access is set to.
//! Each opens one connection, or a second one after a Hello refused for its
//! connection token, and reports back when the last connect finds no router
//! running.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use koshi_core::ids::SessionId;
use koshi_core::text::sanitize_reported_text;
use koshi_ipc::endpoint::{is_update_restarting_servers, EndpointFile, RESTART_WINDOW_DURATION};
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload, ProcessImageId};
use koshi_ipc::router::{
    resolve_router_endpoint_path, resolve_router_program_file_path, IncomingRouterResponse,
    RouterRequest, RouterRequestKind, RouterResult, ROUTER_RESTARTING_MESSAGE,
};
use koshi_ipc::transport::{
    compute_remaining_duration_until, Connection, FrameReader, FrameWriter, CONNECT_WAIT_DURATION,
};

use crate::error::CliError;
use crate::ipc_client::{
    bound_by_answer_deadline, compute_poll_pause_duration, is_answer_deadline_reached,
    REFUSED_SERVER_RESTART_START_WAIT_DURATION, REFUSED_TOKEN_ADVERTISE_WAIT_DURATION,
    RESTART_POLL_INTERVAL_DURATION,
};
use crate::rejoin_claim::{REJOIN_CLIENT_ID_VARIABLE_NAME, REJOIN_TOKEN_VARIABLE_NAME};
use crate::server_build::{find_refusing_server_build, RefusingServerBuild};
use crate::talk::{self, build_ipc_unavailable_error, RouterHelloAnswer};

#[cfg(test)]
mod tests;

/// The subcommand this binary starts itself under to run the router. The
/// arguments after it are [`RUNTIME_DIRECTORY_FLAG`] with the directory to serve,
/// and [`WAIT_FOR_LOCK_FLAG`] when the started router waits for the router
/// lock.
pub const ROUTER_SUBCOMMAND: &str = "serve-router";

/// The flag naming the runtime directory a started process serves. Takes that
/// directory as its value.
pub const RUNTIME_DIRECTORY_FLAG: &str = "--runtime-dir";

/// The flag that tells a started router to wait for the router lock rather
/// than yield to the router holding it.
pub const WAIT_FOR_LOCK_FLAG: &str = "--wait-for-lock";

/// How long a freshly started router has to bind its socket and advertise it
/// before the request gives up.
const ROUTER_START_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// How long the retry loop pauses between connect attempts while it waits for
/// a freshly started router.
const ROUTER_START_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(100);

/// The endpoint file the last run of a [`run_router_exchange`] used, with what
/// that run gave.
type RouterExchangeOutcome<RouterAnswer> = (EndpointFile, Result<RouterAnswer, CliError>);

/// Ask the router for `request_kind` and hand back its answer.
///
/// Tries the exchange once. With no router running it starts one detached and
/// retries every 100 milliseconds until the router answers or 5 seconds pass.
/// When the router it started ends and no router answers yet, it starts one
/// more, which waits for the router lock. No router runs the request on an
/// attempt that finds no router: that attempt sent nothing, the router refused
/// the Hello in front of the request, or the router closed the connection
/// before it answered the Hello. A retried request runs once at most.
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
/// A router that refuses the connection token its endpoint file carries is
/// asked once more when that file carries another token or cannot be read, or
/// after [`REFUSED_TOKEN_ADVERTISE_WAIT_DURATION`].
///
/// The answer is the router's own result for `request_kind`, including a
/// [`RouterResult::Error`] refusing it. A Hello refused for its protocol
/// version is [`CliError::ProtocolVersionRefused`] when the router is not
/// waited for, or did not restart within the wait. Its sentence is the
/// refusal, then `; `, then what
/// [`RefusingServerBuild::format_refusal_hint`](crate::server_build::RefusingServerBuild::format_refusal_hint)
/// gives for the router's program file read at that moment, with the clause
/// `run: koshi restart-servers`. A Hello refused for its connection token on
/// the second ask too is [`CliError::ConnectionTokenRefused`]. A reply in the
/// envelope of koshi 0.4.0 or older is [`CliError::PreviousReleaseServer`].
/// Every other refused Hello, a reply answering nothing that was asked, and
/// every other failure to talk are [`CliError::IpcUnavailable`].
pub fn submit_router_request(
    runtime_directory: &Path,
    request_kind: RouterRequestKind,
) -> Result<RouterResult, CliError> {
    let first_router_exchange =
        run_router_exchange(runtime_directory, None, |connection, router_endpoint| {
            exchange_router_request_on_connection(connection, router_endpoint, &request_kind, None)
        })?;
    let first_router_result = match first_router_exchange {
        None => None,
        Some((router_endpoint, exchange_result)) => match exchange_result {
            Err(CliError::ProtocolVersionRefused {
                detail: refusal_detail,
            }) => {
                let router_program_file_path = resolve_router_program_file_path(runtime_directory);
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
                            refusing_server_build.format_refusal_hint("run: koshi restart-servers")
                        ),
                    }
                };
                let refusing_server_build = find_router_build();
                if !refusing_server_build.is_restart_expected() {
                    return Err(build_refusal_error(&refusing_server_build));
                }
                let restart_deadline = Instant::now() + REFUSED_SERVER_RESTART_START_WAIT_DURATION;
                if !wait_for_router_restart(
                    runtime_directory,
                    &router_endpoint.connection_token,
                    restart_deadline,
                ) {
                    return Err(build_refusal_error(&find_router_build()));
                }
                exchange_router_request(runtime_directory, &request_kind, None)?
            }
            Ok(RouterResult::Error(router_refusal))
                if router_refusal.message == ROUTER_RESTARTING_MESSAGE =>
            {
                let _ = wait_for_router_restart(
                    runtime_directory,
                    &router_endpoint.connection_token,
                    Instant::now() + RESTART_WINDOW_DURATION,
                );
                exchange_router_request(runtime_directory, &request_kind, None)?
            }
            exchange_result => Some(exchange_result?),
        },
    };
    if let Some(router_result) = first_router_result {
        return Ok(router_result);
    }
    start_router_and_exchange(runtime_directory, || {
        exchange_router_request(runtime_directory, &request_kind, None)
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
    if let Some(router_version) = find_running_router_version(runtime_directory, None)? {
        return Ok(router_version);
    }
    start_router_and_exchange(runtime_directory, || {
        find_running_router_version(runtime_directory, None)
    })
}

/// Start the router detached, then run `router_exchange` every
/// [`ROUTER_START_POLL_INTERVAL_DURATION`] until it gives an answer, and hand
/// that answer back. `Ok(None)` from `router_exchange` means no router
/// answered yet.
///
/// When a `router_exchange` gives `Ok(None)` after the started router ended,
/// one more router is started, with [`WAIT_FOR_LOCK_FLAG`]. Example: an idle
/// router removes its endpoint file and still holds the router lock; the
/// router started first finds the lock held and ends, and the second one takes
/// the lock once the idle router lets it go.
///
/// # Errors
/// The failure of a start, the first failure `router_exchange` gives, and
/// [`CliError::IpcUnavailable`] reading `the router did not start` once
/// [`ROUTER_START_TIMEOUT_DURATION`] passes with no answer.
fn start_router_and_exchange<RouterAnswer>(
    runtime_directory: &Path,
    mut router_exchange: impl FnMut() -> Result<Option<RouterAnswer>, CliError>,
) -> Result<RouterAnswer, CliError> {
    let mut started_router = spawn_router_detached(runtime_directory, false)?;
    let mut is_lock_waiting_router_started = false;

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
        let has_started_router_ended = matches!(started_router.try_wait(), Ok(Some(_)));
        if has_started_router_ended && !is_lock_waiting_router_started {
            started_router = spawn_router_detached(runtime_directory, true)?;
            is_lock_waiting_router_started = true;
        }
        std::thread::sleep(ROUTER_START_POLL_INTERVAL_DURATION);
    }
}

/// The result of asking the running router to restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterRestart {
    /// The router answered that it is replacing its own process image.
    Restarting {
        /// The process image the router named in its Hello answer, before the
        /// Restart went out. `None` from a koshi 0.5.0 router.
        asked_process_image_id: Option<ProcessImageId>,
    },
    /// The last connect found no router running, or the router closed the
    /// connection before its Hello answer. No router ran the Restart.
    NotRunning,
    /// The Restart went out, and its answer did not come by the deadline. The
    /// connection is closed when this is handed back. The router restarts
    /// only when it wrote its `Restarting` answer before that close.
    Unanswered {
        /// The process image the router named in its Hello answer, before the
        /// Restart went out. `None` from a koshi 0.5.0 router.
        asked_process_image_id: Option<ProcessImageId>,
    },
}

/// Ask the router that is already running to restart into the binary on disk.
///
/// Writes the Hello, reads its answer, then writes the Restart and reads its
/// answer. After a Hello refused for its connection token, it connects once
/// more when the endpoint file carries another token or cannot be read, or
/// after [`REFUSED_TOKEN_ADVERTISE_WAIT_DURATION`], when 1 ms or more is left
/// until `answer_deadline`. Never starts a router. The connects, the wait, the
/// writes and the reads all end by `answer_deadline`. A Restart write or read
/// that runs past it gives [`RouterRestart::Unanswered`].
///
/// # Errors
/// A router that refuses the Restart is [`CliError::IpcUnavailable`] carrying
/// the sentence the router sent, filtered by [`sanitize_reported_text`]. A
/// router whose build has no Restart kind refuses it, and that sentence names
/// both builds. A refused Hello is what [`talk::build_hello_refusal_error`]
/// gives, and no Restart goes out behind it. An endpoint file that exists and
/// cannot be read, and a connect that fails for a reason other than nothing
/// listening, are their failures. A Hello write or read that fails because the
/// router closed the connection gives [`RouterRestart::NotRunning`]. Any other
/// Hello write or read that fails is [`CliError::RouterAnswerTimedOut`] once
/// `answer_deadline` is reached, and no Restart goes out. Every other write or
/// read that fails is what [`build_ipc_unavailable_error`] gives.
pub fn restart_running_router(
    runtime_directory: &Path,
    answer_deadline: Instant,
) -> Result<RouterRestart, CliError> {
    let restart_exchange = run_router_exchange(
        runtime_directory,
        Some(answer_deadline),
        |connection, router_endpoint| {
            exchange_router_restart_on_connection(connection, router_endpoint, answer_deadline)
        },
    )?;
    match restart_exchange {
        None => Ok(RouterRestart::NotRunning),
        Some((_, router_restart)) => router_restart,
    }
}

/// What the running router answered about this machine's remote access, as a
/// [`RouterResult::RemoteStatus`] answer carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAccessStatus {
    /// The IP address and port the router read from `koshi.kdl` when it
    /// started, or `None` when that file named none.
    pub remote_listen_address: Option<SocketAddr>,
    /// The `koshi.kdl` the router read `remote_listen_address` from, or `None`
    /// when the router names none: a router with no config directory, and a
    /// koshi 0.5.0 router.
    pub config_file_path: Option<PathBuf>,
    /// Whether the operator has switched remote access on.
    pub is_remote_access_enabled: bool,
    /// Whether the router holds the port right now.
    pub is_listening: bool,
    /// The fingerprint of the certificate the open listener presents, as 64
    /// lowercase hex characters, or `None` when the router names none.
    pub certificate_fingerprint: Option<String>,
    /// How many connections from another machine the router holds admitted,
    /// whether they have attached to a session or not.
    pub remote_connection_count: usize,
}

impl RemoteAccessStatus {
    /// The status a [`RouterResult::RemoteStatus`] answer carries.
    ///
    /// # Errors
    /// Every other answer, handed back unchanged.
    pub fn from_router_result(
        router_result: RouterResult,
    ) -> Result<RemoteAccessStatus, RouterResult> {
        match router_result {
            RouterResult::RemoteStatus {
                remote_listen_address,
                config_file_path,
                is_remote_access_enabled,
                is_listening,
                certificate_fingerprint,
                remote_connection_count,
            } => Ok(RemoteAccessStatus {
                remote_listen_address,
                config_file_path,
                is_remote_access_enabled,
                is_listening,
                certificate_fingerprint,
                remote_connection_count,
            }),
            other_router_result => Err(other_router_result),
        }
    }
}

/// What asking the running router for its remote access status produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunningRouterRemoteStatus {
    /// A router answered with its remote access status.
    Answered(RemoteAccessStatus),
    /// No endpoint file, nothing listening behind it, or a router that closed
    /// the connection before its Hello answer.
    NotRunning,
    /// A router is listening and has no request kind by this name.
    OlderBuild,
    /// A router is listening and replies in the envelope of koshi 0.4.0 or
    /// older, which this koshi cannot talk to.
    PreviousRelease,
    /// A router is listening and did not answer the question.
    NoAnswer {
        /// Why there is no status: the sentence the router refused with, the
        /// sentence naming a reply that answers nothing this asked, or the
        /// sentence naming a failure to talk.
        error_detail: String,
        /// The process id `router.json` names, read after the failure, or
        /// `None` when that file cannot be read.
        router_process_id: Option<u32>,
    },
}

/// What the running router's remote access is set to: the address it read
/// from `koshi.kdl`, the file it read it from, whether remote access is on,
/// whether it holds the port, the certificate fingerprint, and how many
/// connections from another machine it holds admitted.
///
/// Sends one RemoteStatus exchange. After a Hello refused for its connection
/// token, it sends one more when the endpoint file carries another token or
/// cannot be read, or after [`REFUSED_TOKEN_ADVERTISE_WAIT_DURATION`]. Never
/// starts a router. The connects, the writes and the reads all end by
/// `answer_deadline`.
///
/// A router whose build has no such request kind refuses it with
/// [`IpcErrorCode::UnsupportedKind`], which is
/// [`RunningRouterRemoteStatus::OlderBuild`]. A router that replies in the
/// envelope of koshi 0.4.0 or older is
/// [`RunningRouterRemoteStatus::PreviousRelease`]. Every other refusal,
/// unexpected reply and transport failure, and a router that has not answered
/// by `answer_deadline`, is [`RunningRouterRemoteStatus::NoAnswer`], carrying
/// the process id `router.json` names at that moment.
///
/// Example: a router that accepts the connection and answers nothing gives
/// `NoAnswer` with the detail `IPC unavailable: the router did not answer in
/// time` at `answer_deadline`.
#[must_use]
pub fn query_running_router_remote_status(
    runtime_directory: &Path,
    answer_deadline: Instant,
) -> RunningRouterRemoteStatus {
    let error_detail = match exchange_router_request(
        runtime_directory,
        &RouterRequestKind::RemoteStatus,
        Some(answer_deadline),
    ) {
        Ok(None) => return RunningRouterRemoteStatus::NotRunning,
        Ok(Some(router_result)) => match RemoteAccessStatus::from_router_result(router_result) {
            Ok(remote_access_status) => {
                return RunningRouterRemoteStatus::Answered(remote_access_status)
            }
            Err(RouterResult::Error(refusal)) if refusal.code == IpcErrorCode::UnsupportedKind => {
                return RunningRouterRemoteStatus::OlderBuild
            }
            Err(RouterResult::Error(refusal)) => refusal.message,
            Err(unexpected_router_result) => talk::ROUTER_PEER_WORDS
                .build_unexpected_reply_error(&unexpected_router_result)
                .to_string(),
        },
        Err(CliError::PreviousReleaseServer { .. }) => {
            return RunningRouterRemoteStatus::PreviousRelease
        }
        Err(router_exchange_error) => router_exchange_error.to_string(),
    };
    RunningRouterRemoteStatus::NoAnswer {
        error_detail,
        router_process_id: EndpointFile::load_from_path(&resolve_router_endpoint_path(
            runtime_directory,
        ))
        .ok()
        .map(|router_endpoint_file| router_endpoint_file.process_id),
    }
}

/// The build version the running router reports in its Hello answer, as
/// [`find_running_router_hello_answer`] reads it.
///
/// # Errors
/// What [`find_running_router_hello_answer`] gives.
pub fn find_running_router_version(
    runtime_directory: &Path,
    answer_deadline: Option<Instant>,
) -> Result<Option<String>, CliError> {
    let router_hello_answer = find_running_router_hello_answer(runtime_directory, answer_deadline)?;
    Ok(router_hello_answer.map(|router_hello_answer| router_hello_answer.build_version))
}

/// The build and the process image the running router names in its Hello
/// answer.
///
/// `Ok(None)` means no router is running, or the router closed the connection
/// before its Hello answer. Sends nothing besides the Hello; never starts a
/// router. A router that refuses the connection token its endpoint file carries
/// is asked once more when that file carries another token or cannot be read,
/// or after [`REFUSED_TOKEN_ADVERTISE_WAIT_DURATION`]. With `answer_deadline`,
/// the connects, the wait, the write and the read all end by that moment, as
/// [`restart_running_router`] states, and a read that runs past it is
/// [`CliError::RouterAnswerTimedOut`].
///
/// # Errors
/// A refused Hello, as [`talk::build_hello_refusal_error`] gives it, a Hello
/// answer naming a version outside the range this build sent or of another
/// kind, which is [`CliError::IpcUnavailable`], and every failure to talk.
pub fn find_running_router_hello_answer(
    runtime_directory: &Path,
    answer_deadline: Option<Instant>,
) -> Result<Option<RouterHelloAnswer>, CliError> {
    match run_router_exchange(
        runtime_directory,
        answer_deadline,
        |connection, router_endpoint| {
            let Some((mut response_reader, _request_writer)) =
                send_router_hello(connection, router_endpoint, answer_deadline)?
            else {
                return Ok(None);
            };
            receive_router_hello_answer(&mut response_reader, answer_deadline)
        },
    )? {
        None => Ok(None),
        Some((_, router_hello_answer)) => router_hello_answer.map(Some),
    }
}

/// Split `connection`, opened to the router `router_endpoint` names, set
/// `answer_deadline` on both halves, and write the Hello with request id `1`,
/// presenting the token `router_endpoint` carries.
///
/// `Ok(None)` means the router closed the connection before the write, as
/// [`find_failure_before_hello_answer`] states.
///
/// # Errors
/// A write that fails otherwise, as [`build_router_exchange_error`] states.
fn send_router_hello(
    connection: Connection,
    router_endpoint: &EndpointFile,
    answer_deadline: Option<Instant>,
) -> Result<Option<(FrameReader, FrameWriter)>, CliError> {
    let (mut response_reader, mut request_writer) = connection.split();
    response_reader.set_deadline(answer_deadline);
    request_writer.set_deadline(answer_deadline);
    let hello_request = RouterRequest {
        request_id: 1,
        request_kind: RouterRequestKind::build_hello_request(
            router_endpoint.connection_token.clone(),
        ),
    };
    if let Err(ipc_error) = request_writer.send(&hello_request) {
        return find_failure_before_hello_answer(ipc_error, answer_deadline);
    }
    Ok(Some((response_reader, request_writer)))
}

/// Read the answer to the Hello [`send_router_hello`] wrote, as
/// [`talk::parse_router_hello_answer`] reads it.
///
/// `Ok(None)` means the router closed the connection with no Hello answer, as
/// [`find_failure_before_hello_answer`] states.
///
/// # Errors
/// A read that fails otherwise, as [`build_router_exchange_error`] states, and
/// what [`talk::parse_router_hello_answer`] gives.
fn receive_router_hello_answer(
    response_reader: &mut FrameReader,
    answer_deadline: Option<Instant>,
) -> Result<Option<RouterHelloAnswer>, CliError> {
    let hello_answer_read: Result<IncomingRouterResponse, IpcError> = response_reader.recv_answer();
    match hello_answer_read {
        Ok(hello_response) => talk::parse_router_hello_answer(hello_response).map(Some),
        Err(ipc_error) => find_failure_before_hello_answer(ipc_error, answer_deadline),
    }
}

/// Write `request_kind` with request id `2`, the request that follows the
/// Hello.
///
/// # Errors
/// The failure of the write.
fn send_router_request(
    request_writer: &mut FrameWriter,
    request_kind: &RouterRequestKind,
) -> Result<(), IpcError> {
    let router_request = RouterRequest {
        request_id: 2,
        request_kind: request_kind.clone(),
    };
    request_writer.send(&router_request)
}

/// Read the answer to the request [`send_router_request`] wrote, from a router
/// that named `router_build_version` in its Hello answer.
///
/// A [`RouterResult::Error`] comes back through
/// [`rewrite_router_result_for_build`], so its message is filtered before any
/// caller reports it.
///
/// # Errors
/// A read that fails, as [`build_router_exchange_error`] states, and an answer
/// that [`talk::ROUTER_PEER_WORDS`] does not take.
fn receive_router_result(
    response_reader: &mut FrameReader,
    router_build_version: &str,
    answer_deadline: Option<Instant>,
) -> Result<RouterResult, CliError> {
    let router_response: IncomingRouterResponse = response_reader
        .recv_answer()
        .map_err(|ipc_error| build_router_exchange_error(ipc_error, answer_deadline))?;
    talk::ROUTER_PEER_WORDS
        .take_response_result(router_response)
        .map(|router_result| rewrite_router_result_for_build(router_result, router_build_version))
}

/// Write the Hello on `connection`, opened to the router `router_endpoint`
/// names, read its answer, then write the Restart and read its answer. The
/// writes and the reads end by `answer_deadline`.
///
/// - `Restarting` gives [`RouterRestart::Restarting`] carrying the process
///   image the Hello answer named.
/// - A Restart write or read that runs past `answer_deadline` gives
///   [`RouterRestart::Unanswered`] carrying that process image.
/// - `Ok(None)` means the router closed the connection before its Hello
///   answer, and no Restart was written.
///
/// # Errors
/// What [`send_router_hello`] and [`receive_router_hello_answer`] give, with no
/// Restart written. A refused Restart, an answer of another kind, and every
/// other failure to talk, as [`restart_running_router`] states.
fn exchange_router_restart_on_connection(
    connection: Connection,
    router_endpoint: &EndpointFile,
    answer_deadline: Instant,
) -> Result<Option<RouterRestart>, CliError> {
    let Some((mut response_reader, mut request_writer)) =
        send_router_hello(connection, router_endpoint, Some(answer_deadline))?
    else {
        return Ok(None);
    };
    let Some(router_hello_answer) =
        receive_router_hello_answer(&mut response_reader, Some(answer_deadline))?
    else {
        return Ok(None);
    };
    let asked_process_image_id = router_hello_answer.process_image_id;
    let restart_result = send_router_request(&mut request_writer, &RouterRequestKind::Restart)
        .map_err(|ipc_error| build_router_exchange_error(ipc_error, Some(answer_deadline)))
        .and_then(|()| {
            receive_router_result(
                &mut response_reader,
                &router_hello_answer.build_version,
                Some(answer_deadline),
            )
        });
    match restart_result {
        Ok(RouterResult::Restarting) => Ok(Some(RouterRestart::Restarting {
            asked_process_image_id,
        })),
        Ok(RouterResult::Error(refusal)) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        Ok(unexpected_router_result) => {
            Err(talk::ROUTER_PEER_WORDS.build_unexpected_reply_error(&unexpected_router_result))
        }
        Err(CliError::RouterAnswerTimedOut) => Ok(Some(RouterRestart::Unanswered {
            asked_process_image_id,
        })),
        Err(restart_exchange_error) => Err(restart_exchange_error),
    }
}

/// The failure for `ipc_error`, which a write or a read of a router exchange
/// under `answer_deadline` gave: [`CliError::RouterAnswerTimedOut`] once
/// `answer_deadline` is reached, as [`is_answer_deadline_reached`] decides,
/// and what [`build_ipc_unavailable_error`] gives otherwise.
fn build_router_exchange_error(ipc_error: IpcError, answer_deadline: Option<Instant>) -> CliError {
    if is_answer_deadline_reached(answer_deadline) {
        return CliError::RouterAnswerTimedOut;
    }
    build_ipc_unavailable_error(ipc_error)
}

/// What `ipc_error` gives when a write or a read on a router connection
/// failed with it before the router's Hello answer was read: `Ok(None)` for
/// [`IpcError::Disconnected`], and the failure [`build_router_exchange_error`]
/// gives under `answer_deadline` for every other error.
///
/// `Ok(None)` means the router closed the connection before this caller read
/// a Hello answer, and ran nothing sent on it. A router answers the Hello
/// before it reads the request behind it. Example: a router whose idle window
/// passed closes each connection it accepts from then on unread.
fn find_failure_before_hello_answer<RouterAnswer>(
    ipc_error: IpcError,
    answer_deadline: Option<Instant>,
) -> Result<Option<RouterAnswer>, CliError> {
    match ipc_error {
        IpcError::Disconnected => Ok(None),
        ipc_error => Err(build_router_exchange_error(ipc_error, answer_deadline)),
    }
}

/// Wait until the router's endpoint file in `runtime_directory` no longer
/// carries `router_connection_token_before_restart`, and return `true`. Return
/// `false` once `restart_deadline` has passed while no `koshi update` holds the
/// update lock in `runtime_directory` ([`is_update_restarting_servers`]).
///
/// A file with another token ends the wait, and so does a file this build
/// cannot read. A missing file and a file with the same token are read again
/// every [`RESTART_POLL_INTERVAL_DURATION`], and before `restart_deadline` no
/// pause runs past `restart_deadline`. The first read happens before the
/// deadline is checked: with a deadline already passed, that one read still
/// sees a file that already changed. Example: a `koshi update` that restarts
/// sessions for 50 seconds before it restarts the router holds the lock all
/// that time, and a wait with a 30-second deadline ends on the router's new
/// token.
#[must_use]
pub fn wait_for_router_restart(
    runtime_directory: &Path,
    router_connection_token_before_restart: &ConnectionToken,
    restart_deadline: Instant,
) -> bool {
    loop {
        if wait_for_router_endpoint_change(
            runtime_directory,
            router_connection_token_before_restart,
            restart_deadline,
        ) {
            return true;
        }
        if !is_update_restarting_servers(runtime_directory) {
            return false;
        }
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

/// Wait until the router's endpoint file in `runtime_directory` no longer
/// carries `connection_token`, and return `true`. Return `false` once
/// `wait_deadline` has passed, whether or not a `koshi update` holds the
/// update lock. The file is read as [`wait_for_router_restart`] states.
fn wait_for_router_endpoint_change(
    runtime_directory: &Path,
    connection_token: &ConnectionToken,
    wait_deadline: Instant,
) -> bool {
    let router_endpoint_path = resolve_router_endpoint_path(runtime_directory);
    loop {
        match EndpointFile::load_from_path(&router_endpoint_path) {
            Ok(router_endpoint) if router_endpoint.connection_token != *connection_token => {
                return true;
            }
            Err(
                IpcError::EndpointFileUnreadable { .. }
                | IpcError::Koshi010WindowEndpointFile { .. },
            ) => return true,
            Ok(_) | Err(_) => {}
        }
        if Instant::now() >= wait_deadline {
            return false;
        }
        std::thread::sleep(compute_poll_pause_duration(wait_deadline));
    }
}

/// Read the router's endpoint file in `runtime_directory`, connect to the
/// address it names, and run `router_exchange` on that connection with that
/// file. Gives back the file the last run used, with what that run gave.
/// `Ok(None)` means the last connect found no router running: the endpoint
/// file is missing, nothing listens at the address it names, or
/// `router_exchange` gave `Ok(None)` because the router closed the connection
/// before its Hello answer. When that is the first connect, no router ran
/// anything sent on it. When it is the second, the router refused the first
/// run's Hello and ran none of the requests behind it.
///
/// A run that gives [`CliError::ConnectionTokenRefused`] met a router that
/// does not accept the token the file carries. This then waits with
/// [`wait_for_router_endpoint_change`] for up to
/// [`REFUSED_TOKEN_ADVERTISE_WAIT_DURATION`], reads the file again, and runs
/// `router_exchange` once more, whatever the wait saw. Example: a router that
/// has bound its socket and not yet written `router.json` refuses the token
/// of the router before it; once it writes `router.json`, the second run
/// presents its token and gets its answer.
///
/// With `answer_deadline`, the wait ends by `answer_deadline`, and a wait that
/// ends with less than 1 ms left hands back the refusal as it is, with no
/// second connect. Each connect waits no longer than the time left, as
/// [`connect_to_running_router`] states.
///
/// # Errors
/// The endpoint file exists and cannot be read, or the connect fails for a
/// reason other than nothing listening.
fn run_router_exchange<RouterAnswer, RouterExchange>(
    runtime_directory: &Path,
    answer_deadline: Option<Instant>,
    mut router_exchange: RouterExchange,
) -> Result<Option<RouterExchangeOutcome<RouterAnswer>>, CliError>
where
    RouterExchange: FnMut(Connection, &EndpointFile) -> Result<Option<RouterAnswer>, CliError>,
{
    let Some((connection, router_endpoint)) =
        connect_to_running_router(runtime_directory, answer_deadline)?
    else {
        return Ok(None);
    };
    let Some(exchange_result) = router_exchange(connection, &router_endpoint).transpose() else {
        return Ok(None);
    };
    if !matches!(
        exchange_result,
        Err(CliError::ConnectionTokenRefused { .. })
    ) {
        return Ok(Some((router_endpoint, exchange_result)));
    }
    let _ = wait_for_router_endpoint_change(
        runtime_directory,
        &router_endpoint.connection_token,
        bound_by_answer_deadline(
            Instant::now() + REFUSED_TOKEN_ADVERTISE_WAIT_DURATION,
            answer_deadline,
        ),
    );
    if is_answer_deadline_reached(answer_deadline) {
        return Ok(Some((router_endpoint, exchange_result)));
    }
    let Some((connection, router_endpoint)) =
        connect_to_running_router(runtime_directory, answer_deadline)?
    else {
        return Ok(None);
    };
    let Some(exchange_result) = router_exchange(connection, &router_endpoint).transpose() else {
        return Ok(None);
    };
    Ok(Some((router_endpoint, exchange_result)))
}

/// A connection to the running router, with the endpoint file that named it.
///
/// `Ok(None)` means no router is running — the endpoint file is missing, or
/// nothing listens at the address it names. Nothing is sent. The connect waits
/// at most [`CONNECT_WAIT_DURATION`], and with `answer_deadline` no longer than
/// the time left until it. The connection is kept only when this OS user
/// serves the address, as [`Connection::connect_to_own_server_within`] checks.
///
/// # Errors
/// The endpoint file exists and cannot be read, the connect fails for a reason
/// other than nothing listening, another OS user serves the address, or less
/// than 1 ms is left until `answer_deadline`: [`CliError::IpcUnavailable`] for
/// each.
fn connect_to_running_router(
    runtime_directory: &Path,
    answer_deadline: Option<Instant>,
) -> Result<Option<(Connection, EndpointFile)>, CliError> {
    let endpoint =
        match EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory)) {
            Ok(endpoint) => endpoint,
            Err(IpcError::EndpointFileMissing { .. }) => return Ok(None),
            Err(ipc_error) => return Err(build_ipc_unavailable_error(ipc_error)),
        };
    let connect_wait_duration = match answer_deadline {
        Some(answer_deadline) => compute_remaining_duration_until(answer_deadline)
            .map_err(|deadline_reached_error| {
                build_ipc_unavailable_error(IpcError::Transport {
                    error_detail: deadline_reached_error.to_string(),
                })
            })?
            .min(CONNECT_WAIT_DURATION),
        None => CONNECT_WAIT_DURATION,
    };
    match Connection::connect_to_own_server_within(&endpoint.socket_address, connect_wait_duration)
    {
        Ok(connection) => Ok(Some((connection, endpoint))),
        Err(IpcError::NoListener { .. }) => Ok(None),
        Err(ipc_error) => Err(build_ipc_unavailable_error(ipc_error)),
    }
}

/// One exchange with a running router: [`exchange_router_request_on_connection`]
/// run through [`run_router_exchange`]. With `answer_deadline`, the connects,
/// the writes and the reads all end by that moment.
///
/// `Ok(None)` means the last connect found no router running, as
/// [`run_router_exchange`] states, and no router ran `request_kind`.
fn exchange_router_request(
    runtime_directory: &Path,
    request_kind: &RouterRequestKind,
    answer_deadline: Option<Instant>,
) -> Result<Option<RouterResult>, CliError> {
    match run_router_exchange(
        runtime_directory,
        answer_deadline,
        |connection, router_endpoint| {
            exchange_router_request_on_connection(
                connection,
                router_endpoint,
                request_kind,
                answer_deadline,
            )
        },
    )? {
        None => Ok(None),
        Some((_, router_result)) => router_result.map(Some),
    }
}

/// Pipeline the Hello and `request_kind` back to back on `connection`, opened
/// to the router `endpoint` names, and read both replies in order. With
/// `answer_deadline`, the writes and the reads end by that moment, and one
/// that runs past it is [`CliError::RouterAnswerTimedOut`]. The answer comes
/// back as [`receive_router_result`] states.
///
/// `Ok(None)` means the router closed the connection before this caller read
/// its Hello answer, as [`find_failure_before_hello_answer`] states, and ran no
/// request: the Hello write, the request write, or the Hello answer read
/// failed with [`IpcError::Disconnected`].
fn exchange_router_request_on_connection(
    connection: Connection,
    endpoint: &EndpointFile,
    request_kind: &RouterRequestKind,
    answer_deadline: Option<Instant>,
) -> Result<Option<RouterResult>, CliError> {
    let Some((mut response_reader, mut request_writer)) =
        send_router_hello(connection, endpoint, answer_deadline)?
    else {
        return Ok(None);
    };
    if let Err(ipc_error) = send_router_request(&mut request_writer, request_kind) {
        return find_failure_before_hello_answer(ipc_error, answer_deadline);
    }
    let Some(router_hello_answer) =
        receive_router_hello_answer(&mut response_reader, answer_deadline)?
    else {
        return Ok(None);
    };
    receive_router_result(
        &mut response_reader,
        &router_hello_answer.build_version,
        answer_deadline,
    )
    .map(Some)
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

/// Start the router as a detached process serving `runtime_directory`, with
/// [`WAIT_FOR_LOCK_FLAG`] when `should_wait_for_lock` holds, and hand back
/// its process.
///
/// It is detached as [`koshi_host::detached_process::configure_detached_process`]
/// sets it: no standard input, output, or error, and a process group of its
/// own. It keeps running after the shell that started it goes away, and writes
/// nothing over the caller's terminal. On Unix, a router that exits while this
/// process runs and no `try_wait` collects it stays a zombie until this
/// process exits.
fn spawn_router_detached(
    runtime_directory: &Path,
    should_wait_for_lock: bool,
) -> Result<Child, CliError> {
    let program_path = koshi_host::program_path::resolve_program_path().map_err(|io_error| {
        CliError::IpcUnavailable {
            detail: format!("this binary's own path could not be read: {io_error}"),
        }
    })?;
    build_router_command(&program_path, runtime_directory, should_wait_for_lock)
        .spawn()
        .map_err(|io_error| CliError::IpcUnavailable {
            detail: format!("the router could not be started: {io_error}"),
        })
}

/// The command `<program_path> serve-router --runtime-dir <runtime_directory>`,
/// with `--wait-for-lock` last when `should_wait_for_lock` holds,
/// detached as [`spawn_router_detached`] states. It runs with this process's
/// environment minus [`REJOIN_CLIENT_ID_VARIABLE_NAME`] and
/// [`REJOIN_TOKEN_VARIABLE_NAME`]. No session the router starts, and no pane
/// in it, receives them.
fn build_router_command(
    program_path: &Path,
    runtime_directory: &Path,
    should_wait_for_lock: bool,
) -> std::process::Command {
    let mut router_command = std::process::Command::new(program_path);
    koshi_host::detached_process::configure_detached_process(&mut router_command)
        .arg(ROUTER_SUBCOMMAND)
        .arg(RUNTIME_DIRECTORY_FLAG)
        .arg(runtime_directory)
        .env_remove(REJOIN_CLIENT_ID_VARIABLE_NAME)
        .env_remove(REJOIN_TOKEN_VARIABLE_NAME);
    if should_wait_for_lock {
        router_command.arg(WAIT_FOR_LOCK_FLAG);
    }
    router_command
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
/// the file. On Windows both have no effect.
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
