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
//! Restarting the running router is the one ask that never starts one. It
//! sends a single exchange, and reports back when no router was running.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use koshi_core::ids::SessionId;
use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    router_endpoint_path, IncomingRouterResponse, RouterRequest, RouterRequestKind, RouterResult,
};
use koshi_ipc::transport::Connection;
use koshi_ipc::wire::WireName;

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
/// Nothing is sent on an attempt that finds no router, so a retried request
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
/// A router that refuses the request is [`CliError::IpcUnavailable`]. An older
/// router refuses it that way, by name, because its build has no such kind.
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

/// The build version the running router reports in its Hello answer.
///
/// `Ok(None)` means no router is running. An empty string means the router
/// answered but predates the version field. Sends nothing besides the Hello;
/// never starts a router.
pub fn running_router_version(runtime_dir: &Path) -> Result<Option<String>, CliError> {
    let endpoint = match EndpointFile::read(&router_endpoint_path(runtime_dir)) {
        Ok(endpoint) => endpoint,
        Err(IpcError::EndpointFileMissing { .. }) => return Ok(None),
        Err(error) => return Err(talk_failed(error)),
    };
    let mut connection = match Connection::connect(&endpoint.socket) {
        Ok(connection) => connection,
        Err(IpcError::NoListener { .. }) => return Ok(None),
        Err(error) => return Err(talk_failed(error)),
    };

    let hello = RouterRequest {
        request_id: 1,
        kind: RouterRequestKind::hello(endpoint.token),
    };
    connection.send(&hello).map_err(talk_failed)?;
    let reply: IncomingRouterResponse = connection.recv().map_err(talk_failed)?;
    match talk::ROUTER.take_result(reply)? {
        RouterResult::Hello {
            protocol_version,
            version,
        } => {
            talk::ROUTER.settled_version(protocol_version)?;
            Ok(Some(version))
        }
        RouterResult::Error(refusal) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        other => Err(talk::ROUTER.unexpected_reply(&other)),
    }
}

/// One exchange with a running router: read its endpoint file, connect,
/// pipeline the Hello and `kind` back to back, and read both replies in order.
///
/// `Ok(None)` means no router is running — the endpoint file is missing, or
/// nothing listens at the address it names — and nothing was sent.
fn exchange(
    runtime_dir: &Path,
    kind: &RouterRequestKind,
) -> Result<Option<RouterResult>, CliError> {
    let endpoint = match EndpointFile::read(&router_endpoint_path(runtime_dir)) {
        Ok(endpoint) => endpoint,
        Err(IpcError::EndpointFileMissing { .. }) => return Ok(None),
        Err(error) => return Err(talk_failed(error)),
    };
    let mut connection = match Connection::connect(&endpoint.socket) {
        Ok(connection) => connection,
        Err(IpcError::NoListener { .. }) => return Ok(None),
        Err(error) => return Err(talk_failed(error)),
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
    let router_version = match talk::ROUTER.take_result(hello_reply)? {
        RouterResult::Hello {
            protocol_version,
            version,
        } => {
            talk::ROUTER.settled_version(protocol_version)?;
            version
        }
        RouterResult::Error(refusal) => {
            return Err(CliError::IpcUnavailable {
                detail: refusal.message,
            })
        }
        other => return Err(talk::ROUTER.unexpected_reply(&other)),
    };

    let reply: IncomingRouterResponse = connection.recv().map_err(talk_failed)?;
    talk::ROUTER
        .take_result(reply)
        .map(|result| Some(name_other_build(result, &router_version)))
}

/// A request kind the router does not have, restated to name both builds.
///
/// Only [`IpcErrorCode::UnsupportedKind`] is restated, and only when the
/// router reports a build other than this one: that pairing is a request this
/// koshi has and the running router does not. Every other answer, every other
/// refusal, and every refusal from a router on this build passes through
/// unchanged.
///
/// `router_version` is the build the router reported in its Hello, empty when
/// the router predates that field.
fn name_other_build(result: RouterResult, router_version: &str) -> RouterResult {
    let this_version = env!("CARGO_PKG_VERSION");
    let RouterResult::Error(refusal) = result else {
        return result;
    };
    if refusal.code != IpcErrorCode::UnsupportedKind || router_version == this_version {
        return RouterResult::Error(refusal);
    }
    let running = if router_version.is_empty() {
        "an older koshi that does not report its build".to_string()
    } else {
        format!("koshi {router_version}")
    };
    RouterResult::Error(IpcErrorPayload {
        code: refusal.code,
        message: format!(
            "{} — the running router is {running} and this command is koshi {this_version}; the \
             router serves its own build until it restarts, which it does once no session is \
             left running",
            refusal.message
        ),
    })
}

/// Start the router as a detached process serving `runtime_dir`.
///
/// It gets no standard input, output, or error, and a process group of its
/// own, so it keeps running after the shell that started it goes away and
/// writes nothing over the caller's terminal.
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
        other => Err(CliError::IpcUnavailable {
            detail: format!(
                "the router answered a create session with {}",
                other.wire_name()
            ),
        }),
    }
}
