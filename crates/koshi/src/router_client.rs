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

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::error::IpcError;
use koshi_ipc::router::{
    router_endpoint_path, RouterRequest, RouterRequestKind, RouterResponse, RouterResult,
    ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;

use crate::error::CliError;

#[cfg(test)]
mod tests;

/// The subcommand this binary starts itself under to run the router. The
/// argument after it is [`RUNTIME_DIR_FLAG`] with the directory to serve.
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
        kind: RouterRequestKind::Hello {
            protocol_version: ROUTER_PROTOCOL_VERSION,
            token: endpoint.token,
        },
    };
    let request = RouterRequest {
        request_id: 2,
        kind: kind.clone(),
    };
    connection.send(&hello).map_err(talk_failed)?;
    connection.send(&request).map_err(talk_failed)?;

    let hello_reply: RouterResponse = connection.recv().map_err(talk_failed)?;
    match hello_reply.result {
        RouterResult::Hello => {}
        RouterResult::Error(refusal) => {
            return Err(CliError::IpcUnavailable {
                detail: refusal.message,
            })
        }
        other => return Err(unexpected_reply(&other)),
    }

    let reply: RouterResponse = connection.recv().map_err(talk_failed)?;
    Ok(Some(reply.result))
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

/// A transport failure mid-exchange: the router was reachable but the
/// conversation could not finish.
fn talk_failed(error: IpcError) -> CliError {
    CliError::IpcUnavailable {
        detail: error.to_string(),
    }
}

/// The router answered with a result kind the request cannot produce —
/// a protocol violation, not a control-plane outcome.
fn unexpected_reply(result: &RouterResult) -> CliError {
    let kind = match result {
        RouterResult::Hello => "Hello",
        RouterResult::Created(_) => "Created",
        RouterResult::Found(_) => "Found",
        RouterResult::Sessions(_) => "Sessions",
        RouterResult::Killed => "Killed",
        RouterResult::Error(_) => "Error",
    };
    CliError::IpcUnavailable {
        detail: format!("the router answered with an unexpected {kind} reply"),
    }
}
