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
    router_endpoint_path, IncomingRouterResponse, RouterRequest, RouterRequestKind, RouterResult,
};
use koshi_ipc::transport::Connection;

use crate::error::CliError;
use crate::talk::{self, talk_failed};

#[cfg(test)]
mod tests;

/// The subcommand this binary starts itself under to run the router. The
/// arguments after it are [`RUNTIME_DIR_FLAG`] with the directory to serve,
/// and `--wait-for-lock` when a router hands its place to a replacement.
pub const ROUTER_SUBCOMMAND: &str = "serve-router";

/// The flag naming the runtime directory a started process serves. Takes that
/// directory as its value.
pub const RUNTIME_DIR_FLAG: &str = "--runtime-dir";

/// How long a freshly started router has to bind its socket and advertise it
/// before the request gives up.
const ROUTER_START_WAIT: Duration = Duration::from_secs(5);

/// How long the retry loop pauses between connect attempts while it waits for
/// a freshly started router.
const ROUTER_START_POLL: Duration = Duration::from_millis(100);

/// The Win32 `DETACHED_PROCESS` creation flag: the started process gets no
/// console and does not inherit the caller's.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// The Win32 `CREATE_NEW_PROCESS_GROUP` creation flag: the started process
/// begins a process group of its own.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Ask the router `kind` and hand back its answer.
///
/// Tries the exchange once. With no router running it starts one detached and
/// retries every 100 milliseconds until the router answers or 5 seconds pass.
/// Nothing is sent on an attempt that finds no router: a retried request
/// reaches a router exactly once.
///
/// The answer is the router's own result for `kind`, including a
/// [`RouterResult::Error`] refusing it. A refused Hello, a reply answering
/// nothing that was asked, and every failure to talk are
/// [`CliError::IpcUnavailable`].
pub fn router_request(
    runtime_dir: &Path,
    kind: RouterRequestKind,
) -> Result<RouterResult, CliError> {
    if let Some(result) = exchange(runtime_dir, &kind)? {
        return Ok(result);
    }
    spawn_router_detached(runtime_dir)?;

    let deadline = Instant::now() + ROUTER_START_WAIT;
    loop {
        if let Some(result) = exchange(runtime_dir, &kind)? {
            return Ok(result);
        }
        if Instant::now() >= deadline {
            return Err(CliError::IpcUnavailable {
                detail: "the router did not start".to_string(),
            });
        }
        std::thread::sleep(ROUTER_START_POLL);
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
pub fn restart_running_router(runtime_dir: &Path) -> Result<bool, CliError> {
    match exchange(runtime_dir, &RouterRequestKind::Restart)? {
        None => Ok(false),
        Some(RouterResult::Restarting) => Ok(true),
        Some(RouterResult::Error(refusal)) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        Some(other) => Err(talk::ROUTER.unexpected_reply(&other)),
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
        detail: String,
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
pub fn running_router_remote_connections(runtime_dir: &Path) -> RemoteConnections {
    match exchange(runtime_dir, &RouterRequestKind::RemoteStatus) {
        Ok(None) => RemoteConnections::NotRunning,
        Ok(Some(RouterResult::RemoteStatus {
            remote_connections, ..
        })) => RemoteConnections::Answered(remote_connections),
        Ok(Some(RouterResult::Error(refusal))) if refusal.code == IpcErrorCode::UnsupportedKind => {
            RemoteConnections::OlderBuild
        }
        Ok(Some(RouterResult::Error(refusal))) => RemoteConnections::NoAnswer {
            detail: refusal.message,
        },
        Ok(Some(other)) => RemoteConnections::NoAnswer {
            detail: talk::ROUTER.unexpected_reply(&other).to_string(),
        },
        Err(error) => RemoteConnections::NoAnswer {
            detail: error.to_string(),
        },
    }
}

/// The build version the running router reports in its Hello answer.
///
/// `Ok(None)` means no router is running. An empty string means the router
/// answered but predates the version field. Sends nothing besides the Hello;
/// never starts a router.
pub fn running_router_version(runtime_dir: &Path) -> Result<Option<String>, CliError> {
    let Some((mut connection, endpoint)) = open_router(runtime_dir)? else {
        return Ok(None);
    };
    let hello = RouterRequest {
        request_id: 1,
        kind: RouterRequestKind::hello(endpoint.token),
    };
    connection.send(&hello).map_err(talk_failed)?;
    let reply: IncomingRouterResponse = connection.recv().map_err(talk_failed)?;
    talk::router_hello_version(reply).map(Some)
}

/// A connection to the running router, with the endpoint file that named it.
///
/// `Ok(None)` means no router is running — the endpoint file is missing, or
/// nothing listens at the address it names. Nothing is sent.
fn open_router(runtime_dir: &Path) -> Result<Option<(Connection, EndpointFile)>, CliError> {
    let endpoint = match EndpointFile::read(&router_endpoint_path(runtime_dir)) {
        Ok(endpoint) => endpoint,
        Err(IpcError::EndpointFileMissing { .. }) => return Ok(None),
        Err(error) => return Err(talk_failed(error)),
    };
    match Connection::connect(&endpoint.socket) {
        Ok(connection) => Ok(Some((connection, endpoint))),
        Err(IpcError::NoListener { .. }) => Ok(None),
        Err(error) => Err(talk_failed(error)),
    }
}

/// One exchange with a running router: read its endpoint file, connect,
/// pipeline the Hello and `kind` back to back, and read both replies in order.
///
/// A [`RouterResult::Error`] comes back through [`name_other_build`], so its
/// message is filtered before any caller reports it.
///
/// `Ok(None)` means no router is running — the endpoint file is missing, or
/// nothing listens at the address it names — and nothing was sent.
fn exchange(
    runtime_dir: &Path,
    kind: &RouterRequestKind,
) -> Result<Option<RouterResult>, CliError> {
    let Some((mut connection, endpoint)) = open_router(runtime_dir)? else {
        return Ok(None);
    };
    let hello = RouterRequest {
        request_id: 1,
        kind: RouterRequestKind::hello(endpoint.token),
    };
    let request = RouterRequest {
        request_id: 2,
        kind: kind.clone(),
    };
    connection.send(&hello).map_err(talk_failed)?;
    connection.send(&request).map_err(talk_failed)?;

    let hello_reply: IncomingRouterResponse = connection.recv().map_err(talk_failed)?;
    let router_version = talk::router_hello_version(hello_reply)?;

    let reply: IncomingRouterResponse = connection.recv().map_err(talk_failed)?;
    talk::ROUTER
        .take_result(reply)
        .map(|result| Some(name_other_build(result, &router_version)))
}

/// A refusal filtered by [`sanitize_reported_text`], and — for a request kind
/// the router does not have — restated to name both builds.
///
/// Every refusal passes through this, so the sentence a caller reports carries
/// no control, bidi-control or tag character, and no more than
/// [`MAX_REPORTED_TEXT_BYTES`](koshi_core::text::MAX_REPORTED_TEXT_BYTES) of
/// what the router sent.
///
/// Only [`IpcErrorCode::UnsupportedKind`] is restated, and only when the
/// router reports a build other than this one. Every answer that is not a
/// refusal passes through unchanged.
///
/// `router_version` is the build the router reported in its Hello, empty when
/// the router predates that field.
fn name_other_build(result: RouterResult, router_version: &str) -> RouterResult {
    let this_version = env!("CARGO_PKG_VERSION");
    let RouterResult::Error(refusal) = result else {
        return result;
    };
    let message = sanitize_reported_text(&refusal.message);
    if refusal.code != IpcErrorCode::UnsupportedKind || router_version == this_version {
        return RouterResult::Error(IpcErrorPayload {
            code: refusal.code,
            message,
        });
    }
    let running = if router_version.is_empty() {
        "an older koshi that does not report its build".to_string()
    } else {
        format!("koshi {router_version}")
    };
    RouterResult::Error(IpcErrorPayload {
        code: refusal.code,
        message: format!(
            "{message} — the running router is {running} and this command is koshi \
             {this_version}; the router serves its own build until it restarts, which it does \
             once no session is left running"
        ),
    })
}

/// Start the router as a detached process serving `runtime_dir`.
///
/// It gets no standard input, output, or error, and a process group of its
/// own: it keeps running after the shell that started it goes away, and writes
/// nothing over the caller's terminal.
fn spawn_router_detached(runtime_dir: &Path) -> Result<(), CliError> {
    let exe = std::env::current_exe().map_err(|error| CliError::IpcUnavailable {
        detail: format!("this binary's own path could not be read: {error}"),
    })?;
    let mut command = std::process::Command::new(exe);
    command
        .arg(ROUTER_SUBCOMMAND)
        .arg(RUNTIME_DIR_FLAG)
        .arg(runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    // ponytail: the child handle is dropped, so on Unix a router that exits
    // while this process is still running stays a zombie until this process
    // exits; keep the handle and collect it if a long-lived caller ever starts
    // routers repeatedly.
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| CliError::IpcUnavailable {
            detail: format!("the router could not be started: {error}"),
        })
}

/// Ask the router to make a new session and hand back its id. Starts a router
/// first when none is running.
///
/// The session's first shell opens in the directory this command was run in.
/// A directory that cannot be read is sent as `None`, and the session server
/// keeps the directory it inherited.
///
/// `allow_other_users` `Some(true)` lets the other users of this machine reach
/// the new session whatever its `koshi.kdl` says; `None` leaves that answer to
/// the file.
///
/// # Errors
/// Returns [`CliError::IpcUnavailable`] when the router refuses the create or
/// answers with anything other than the new session.
pub fn request_new_session(
    runtime_dir: &Path,
    profile: Option<&str>,
    allow_other_users: Option<bool>,
) -> Result<SessionId, CliError> {
    let kind = RouterRequestKind::CreateSession {
        profile: profile.map(str::to_string),
        cwd: std::env::current_dir().ok(),
        allow_other_users,
    };
    match router_request(runtime_dir, kind)? {
        RouterResult::Created(address) => Ok(address.id),
        RouterResult::Error(refusal) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        other => Err(talk::ROUTER.unexpected_reply(&other)),
    }
}
