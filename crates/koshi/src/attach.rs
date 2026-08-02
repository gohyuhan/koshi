//! The attached client: join a running session over its control socket and
//! read its event stream until the connection ends.
//!
//! The router resolves the value after `--attach` to one session's address,
//! starting a router first when none runs. The session's endpoint file holds
//! the token the Hello presents; the Hello and the Attach are written back to
//! back, so joining costs one round trip.
//!
//! Everything that can refuse the join happens before the terminal changes
//! mode: a refused lookup, a refused Hello, a refused Attach. Once the session
//! answers `Attached`, the terminal enters raw mode and the alternate screen
//! behind the same cleanup guard the interactive launch uses, so every way out
//! — a detach, the session ending, a dead session server, or a panic — leaves
//! the outer terminal as it was found.
//!
//! This loop paints nothing yet: it reads frames and classifies how the stream
//! ended.

use std::io;
use std::path::Path;

use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{enable_raw_mode, size, EnterAlternateScreen};

use koshi_core::geometry::Size;
use koshi_core::ids::SessionId;
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::protocol::{
    ConnectionToken, EventFilterSpec, IpcRequest, IpcRequestKind, IpcResponse, IpcResult,
    PROTOCOL_VERSION,
};
use koshi_ipc::router::{RouterRequestKind, RouterResult, SessionAddress, SessionSelector};
use koshi_ipc::transport::Connection;
use koshi_observability::cleanup::{install_panic_hook, TerminalCleanupGuard};

use crate::cli::parse_prefixed_uuid;
use crate::error::CliError;
use crate::ipc_client;
use crate::router_client::router_request;

#[cfg(test)]
mod tests;

/// The size an attaching client reports when the terminal size cannot be read,
/// which is what a `koshi --attach` with redirected output finds.
const FALLBACK_VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// How an attached client's event stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// The server detached this client. The session keeps running.
    Detached,
    /// The session shut down and said so before closing.
    SessionEnded,
    /// The connection broke: the session server is gone.
    Died,
}

/// Attach this terminal to the session `selector` names and read its events
/// until the session detaches this client, the session ends, or the connection
/// breaks.
///
/// `selector` is a `session-<uuid>` id, a bare UUID, or a session display
/// name. A broken connection reports the cause and how to reattach, and exits
/// non-zero; the other two endings print what happened and exit zero.
pub fn run(selector: &str) -> Result<(), CliError> {
    let runtime_dir = ipc_client::runtime_dir()?;
    let address = lookup(&runtime_dir, selector)?;
    let endpoint = ipc_client::read_endpoint(&runtime_dir, address.id)?;
    let mut connection = ipc_client::connect(&endpoint, address.id)?;
    let session_id = join(&mut connection, &endpoint.token)?;

    // The session accepted the client, so the terminal may change mode now.
    // The hooks undo every mode this function sets — the same restore body the
    // interactive launch registers — and the panic hook shares them, so an
    // unwinding panic restores the terminal too.
    let cleanup = TerminalCleanupGuard::new();
    crate::app::register_terminal_restore(&cleanup);
    let _panic_guard = install_panic_hook(&cleanup);
    // This client paints nothing yet, so a terminal that refuses either mode
    // still streams: the failure is logged and the loop runs on.
    let _ =
        enable_raw_mode().inspect_err(|error| tracing::warn!(%error, "could not enter raw mode"));
    let _ = execute!(io::stdout(), EnterAlternateScreen)
        .inspect_err(|error| tracing::warn!(%error, "could not enter the alternate screen"));

    let ending = loop {
        if let Some(ending) = classify(&connection.recv::<SessionEvent>()) {
            break ending;
        }
    };

    // Restore the terminal before anything is printed, so the message lands on
    // the shell's own screen rather than the alternate one.
    drop(cleanup);
    report(ending, session_id)
}

/// Ask the router where the session `selector` names listens, starting a
/// router first when none is running.
///
/// A value that reads as a session id (`session-<uuid>` or a bare UUID) is
/// that id; anything else is a display name for the router to match.
fn lookup(runtime_dir: &Path, selector: &str) -> Result<SessionAddress, CliError> {
    let selector = match parse_prefixed_uuid(selector, "session") {
        Ok(uuid) => SessionSelector::Id(SessionId::from_uuid(uuid)),
        Err(_) => SessionSelector::Name(selector.to_string()),
    };
    match router_request(runtime_dir, RouterRequestKind::AttachLookup { selector })? {
        RouterResult::Found(address) => Ok(address),
        RouterResult::Error(refusal) => Err(CliError::IpcUnavailable {
            detail: refusal.message,
        }),
        other => Err(CliError::IpcUnavailable {
            detail: format!("the router answered an attach lookup with {other:?}"),
        }),
    }
}

/// Join the session on an open connection: write the Hello and the Attach back
/// to back, then read both replies in order. Returns the session the server
/// says this client joined.
///
/// The client names no identity of its own — the server mints the client id
/// and answers with it — so only the session id is kept here.
fn join(connection: &mut Connection, token: &ConnectionToken) -> Result<SessionId, CliError> {
    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            protocol_version: PROTOCOL_VERSION,
            token: token.clone(),
        },
    };
    let attach = IpcRequest {
        request_id: 2,
        kind: IpcRequestKind::Attach {
            viewport: viewport(),
            filter: EventFilterSpec::All,
        },
    };
    connection.send(&hello).map_err(ipc_client::talk_failed)?;
    connection.send(&attach).map_err(ipc_client::talk_failed)?;

    let hello_reply: IpcResponse = connection.recv().map_err(ipc_client::talk_failed)?;
    match hello_reply.result {
        IpcResult::Hello => {}
        IpcResult::Error(refusal) => return Err(ipc_client::refused(&refusal)),
        other => return Err(ipc_client::unexpected_reply(&other)),
    }

    let attach_reply: IpcResponse = connection.recv().map_err(ipc_client::talk_failed)?;
    match attach_reply.result {
        IpcResult::Attached { session_id, .. } => Ok(session_id),
        IpcResult::Error(refusal) => Err(ipc_client::refused(&refusal)),
        other => Err(ipc_client::unexpected_reply(&other)),
    }
}

/// This terminal's size in cells, or [`FALLBACK_VIEWPORT`] when it has none to
/// report.
fn viewport() -> Size {
    match size() {
        Ok((cols, rows)) => Size { cols, rows },
        Err(error) => {
            tracing::warn!(%error, "could not read the terminal size");
            FALLBACK_VIEWPORT
        }
    }
}

/// Classify one frame read from the event stream. `None` keeps the loop
/// reading; `Some` ends it.
///
/// Any failure to read is the same ending: the peer closing the socket — which
/// is what a session server exiting or being killed does — surfaces as a read
/// error, so no timeout is involved.
fn classify(frame: &Result<SessionEvent, IpcError>) -> Option<Ending> {
    match frame {
        Ok(SessionEvent::Detached) => Some(Ending::Detached),
        Ok(SessionEvent::Quit) => Some(Ending::SessionEnded),
        Ok(_) => None,
        Err(_) => Some(Ending::Died),
    }
}

/// Print how the stream ended and hand back the process outcome: a broken
/// connection names the cause and how to reattach, and exits non-zero.
fn report(ending: Ending, session_id: SessionId) -> Result<(), CliError> {
    match ending {
        Ending::Detached => {
            println!("detached from session {session_id}");
            Ok(())
        }
        Ending::SessionEnded => {
            println!("the session ended");
            Ok(())
        }
        Ending::Died => Err(CliError::Runtime {
            detail: format!(
                "the session ended unexpectedly\n  \
                 run `koshi list-sessions`; if session {session_id} is still listed, \
                 reattach with `koshi --attach {session_id}`"
            ),
        }),
    }
}
