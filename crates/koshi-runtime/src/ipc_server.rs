//! The control-socket server: how a running koshi answers its socket.
//!
//! [`IpcServer::start`] binds the session's control-socket address, writes
//! the endpoint file advertising it, and spawns the accept loop. Each
//! accepted connection gets its own thread holding its own
//! [`Handshake`] gate: a Hello must open the connection before any other
//! request is served. A `SubmitCommand`, `Discovery`, `Layout` or `Attach`
//! request crosses to the dispatcher thread through the runtime inbox with a
//! reply channel; the dispatcher's answer comes back on it and leaves as the
//! connection's response frame.
//!
//! An `Attach` is the one request that keeps its connection: once the reply
//! carrying the session's structure is written, the connection is split. The
//! writing half carries that client's event stream, and the reading half
//! carries that client's key presses, resizes, pasted text and commands to the
//! dispatcher and writes nothing back. The peer going away detaches the client.
//!
//! A request kind this build does not have is answered `UnsupportedKind` by
//! name, and the connection keeps serving.
//!
//! A connection fault stays on its connection: a malformed-but-aligned
//! frame is answered with `MalformedRequest` and the connection keeps
//! serving, while an oversize frame — whose payload cannot be skipped, so
//! the stream's framing is lost — closes that one connection. Neither
//! reaches the session, any pane, or any other connection.
//!
//! [`IpcServer::shutdown`] stops accepting, joins the accept loop, and
//! removes the endpoint file and the socket, so nothing advertises a
//! session that is gone.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use koshi_core::ids::{ClientId, SessionId};
use koshi_ipc::endpoint::{remove_socket_file, socket_addr, EndpointFile};
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::handshake::Handshake;
use koshi_ipc::protocol::{
    ConnectionToken, IncomingRequest, IpcErrorCode, IpcErrorPayload, IpcRequestKind, IpcResponse,
    IpcResult,
};
use koshi_ipc::transport::{Connection, Listener};
use koshi_ipc::validate::{reclaim_stale_socket, validate_socket_addr};
use koshi_ipc::wire::MaybeKnown;
use koshi_renderer::snapshot::Delivery;

use crate::runtime::bus::wire_event;
use crate::runtime::event::RuntimeEvent;

/// How long the accept loop pauses after a failed accept before trying
/// again, so a persistent accept error (say, the process is out of file
/// descriptors) cannot spin a core.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The serving side of one session's control socket: the bound listener's
/// accept loop, the address it serves, and the endpoint file advertising it.
///
/// Held by the server for the session's lifetime; [`shutdown`](Self::shutdown)
/// stops the loop and withdraws both files.
#[derive(Debug)]
pub struct IpcServer {
    /// The control-socket address the accept loop is serving.
    addr: String,
    /// The endpoint file advertising `addr` and the connection token.
    endpoint_path: PathBuf,
    /// Set by [`shutdown`](Self::shutdown); the accept loop exits when it
    /// observes the flag.
    shutting_down: Arc<AtomicBool>,
    /// The accept loop, joined at shutdown. `Option` so shutdown can take it
    /// out of the otherwise-borrowed struct.
    accept_thread: Option<JoinHandle<()>>,
}

impl IpcServer {
    /// Bind `session`'s control socket inside `runtime_dir`, write the
    /// endpoint file advertising it, and start serving.
    ///
    /// The steps run in trust order: the runtime directory is created
    /// private (`0700`), the address is checked against it, any stale
    /// leftover socket is reclaimed, the listener binds, and only then is
    /// the endpoint file written — so the advertisement never exists without
    /// a listener behind it. A failed endpoint write unwinds the bind and
    /// leaves nothing behind.
    pub fn start(
        runtime_dir: &Path,
        session: SessionId,
        inbox_tx: Sender<RuntimeEvent>,
    ) -> Result<IpcServer, IpcError> {
        koshi_paths::ensure_private_dir(runtime_dir).map_err(|error| IpcError::Transport {
            detail: format!(
                "could not create the runtime directory {}: {error}",
                runtime_dir.display()
            ),
        })?;
        let addr = socket_addr(runtime_dir, session);
        validate_socket_addr(&addr, runtime_dir)?;
        reclaim_stale_socket(&addr)?;
        let listener = Listener::bind(&addr)?;

        let token = ConnectionToken::generate();
        let endpoint_path = EndpointFile::path(runtime_dir, session);
        let endpoint = EndpointFile {
            socket: addr.clone(),
            token: token.clone(),
            pid: std::process::id(),
        };
        if let Err(error) = endpoint.write(&endpoint_path) {
            // Dropping the listener releases the address (and unlinks the
            // socket file on Unix), so the failed start leaves nothing.
            drop(listener);
            remove_socket_file(&addr);
            return Err(error);
        }

        let shutting_down = Arc::new(AtomicBool::new(false));
        let accept_flag = Arc::clone(&shutting_down);
        let accept_thread = std::thread::spawn(move || {
            accept_loop(&listener, &token, &inbox_tx, &accept_flag);
        });

        Ok(IpcServer {
            addr,
            endpoint_path,
            shutting_down,
            accept_thread: Some(accept_thread),
        })
    }

    /// The control-socket address this server is serving.
    #[must_use]
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Stop serving: no further connection is accepted, the accept loop is
    /// joined, and the endpoint file and socket are removed. Connections
    /// already being served run out on their own threads; with the
    /// dispatcher draining, their in-flight requests end in a closed
    /// connection rather than a mutation.
    ///
    /// Dropping an `IpcServer` runs the same teardown, so a path that never
    /// reaches an explicit shutdown — a panic unwinding the server — still
    /// withdraws the files.
    pub fn shutdown(self) {
        // Teardown lives in `Drop`, so consuming `self` is the whole job.
        drop(self);
    }

    /// The teardown itself, safe to run at most once per field: the join is
    /// guarded by taking `accept_thread`, and removing an already-removed
    /// file is a no-op.
    fn stop(&mut self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_thread.take() {
            // The accept loop sits blocked in `accept`; a bare connect wakes it
            // so it observes the flag. Hold that connection open across the join:
            // on Windows a connect that drops before `accept` runs can leave
            // nothing for `accept` to return, so the pending client must outlive
            // the join. A failed connect — say, the process is out of file
            // descriptors — leaves the loop blocked, so the join is skipped
            // rather than waiting forever: the thread dies with the process, and
            // the files below are removed either way.
            if let Ok(wake) = Connection::connect(&self.addr) {
                let _ = handle.join();
                drop(wake);
            }
        }
        let _ = std::fs::remove_file(&self.endpoint_path);
        remove_socket_file(&self.addr);
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Accept connections until the shutdown flag is set, giving each its own
/// serving thread. A failed accept pauses briefly and retries, so one
/// refused connection cannot stop the socket answering.
fn accept_loop(
    listener: &Listener,
    token: &ConnectionToken,
    inbox_tx: &Sender<RuntimeEvent>,
    shutting_down: &AtomicBool,
) {
    loop {
        let connection = listener.accept();
        if shutting_down.load(Ordering::SeqCst) {
            break;
        }
        match connection {
            Ok(connection) => {
                let token = token.clone();
                let inbox_tx = inbox_tx.clone();
                std::thread::spawn(move || serve_connection(connection, token, &inbox_tx));
            }
            Err(_) => std::thread::sleep(ACCEPT_RETRY_DELAY),
        }
    }
}

/// Serve one connection until its peer hangs up or a fault closes it: a
/// [`Handshake`] gates every request, `SubmitCommand`, `Attach` and
/// `Discovery` cross to the dispatcher over the inbox and answer with its
/// reply, and a malformed-but-aligned frame is answered with
/// [`IpcErrorCode::MalformedRequest`] while the connection keeps serving.
///
/// An answered `Attach` ends the request loop: the connection is handed to
/// [`stream_events`], which carries that client's events out and that client's
/// input in for as long as the connection lives.
fn serve_connection(
    mut connection: Connection,
    token: ConnectionToken,
    inbox_tx: &Sender<RuntimeEvent>,
) {
    let mut gate = Handshake::new(token);
    loop {
        let request: IncomingRequest = match connection.recv() {
            Ok(request) => request,
            Err(IpcError::MalformedFrame { .. }) => {
                // The frame was read whole, so the stream is still aligned;
                // only its bytes were unreadable. `request_id: None` tells
                // the caller the answer belongs to no request of its own.
                let refusal = IpcResponse {
                    request_id: None,
                    result: IpcResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::MalformedRequest,
                        message: "the bytes received are not a request this build can read"
                            .to_string(),
                    }),
                };
                if connection.send(&refusal).is_err() {
                    return;
                }
                continue;
            }
            // An oversize frame's payload was never read, so the stream's
            // framing is lost; disconnects and transport faults have no
            // stream left. All close this one connection.
            Err(_) => return,
        };

        let request_id = Some(request.request_id);
        // A kind this build does not have comes from a newer koshi. It is
        // refused by name and the connection keeps serving, so one unfamiliar
        // request does not cost the caller its other verbs.
        let kind = match request.kind {
            MaybeKnown::Known(kind) => kind,
            MaybeKnown::Unknown { name } => {
                let refusal = IpcResponse {
                    request_id,
                    result: IpcResult::Error(gate.refuse_unknown(&name)),
                };
                if connection.send(&refusal).is_err() {
                    return;
                }
                continue;
            }
        };

        let response = match gate.check(&kind) {
            Err(refusal) => IpcResponse {
                request_id,
                result: IpcResult::Error(refusal),
            },
            Ok(()) => match kind {
                IpcRequestKind::Hello { .. } => IpcResponse {
                    request_id,
                    result: IpcResult::Hello {
                        protocol_version: gate
                            .agreed()
                            .expect("an accepted Hello settles the connection's version"),
                    },
                },
                IpcRequestKind::SubmitCommand(envelope) => {
                    let answer = ask_dispatcher(inbox_tx, |reply| RuntimeEvent::Ipc {
                        envelope: *envelope,
                        reply,
                    });
                    match answer {
                        Some(result) => IpcResponse {
                            request_id,
                            result: IpcResult::CommandResult(result),
                        },
                        None => return,
                    }
                }
                IpcRequestKind::Attach { viewport, filter } => {
                    let answer = ask_dispatcher(inbox_tx, |reply| RuntimeEvent::IpcAttach {
                        viewport,
                        filter: filter.into(),
                        attached_at: SystemTime::now(),
                        reply,
                    });
                    // No running session, or no dispatcher left to mint the
                    // client: the process is past its last session, so the
                    // socket is as good as gone.
                    let Some(Some(accepted)) = answer else {
                        return;
                    };
                    let attached = IpcResponse {
                        request_id,
                        result: IpcResult::Attached {
                            client_id: accepted.client_id,
                            session_id: accepted.session_id,
                            structure: accepted.structure,
                        },
                    };
                    if connection.send(&attached).is_err() {
                        let _ = inbox_tx.send(RuntimeEvent::ClientDetached {
                            client_id: accepted.client_id,
                        });
                        return;
                    }
                    // The reply is written; from here the connection carries
                    // the client's event stream and the client's own input.
                    stream_events(connection, accepted.client_id, accepted.events, inbox_tx);
                    return;
                }
                // A key press, a resize, a paste and a mouse round belong on an
                // attached client's connection, which the `Attach` arm above
                // hands to `stream_events`. On this control path they name no
                // client, so they close the connection.
                IpcRequestKind::KeyPress { .. }
                | IpcRequestKind::Resize { .. }
                | IpcRequestKind::Paste { .. }
                | IpcRequestKind::Mouse(_) => return,
                IpcRequestKind::Discovery => {
                    let answer =
                        ask_dispatcher(inbox_tx, |reply| RuntimeEvent::IpcDiscovery { reply });
                    match answer {
                        Some(Some(overview)) => IpcResponse {
                            request_id,
                            result: IpcResult::Overview(overview),
                        },
                        // No running session: the process is past its last
                        // session, so the socket is as good as gone.
                        Some(None) | None => return,
                    }
                }
                IpcRequestKind::Layout { tab } => {
                    let answer =
                        ask_dispatcher(inbox_tx, |reply| RuntimeEvent::IpcLayout { tab, reply });
                    match answer {
                        Some(Some(layout)) => IpcResponse {
                            request_id,
                            result: IpcResult::Layout(layout),
                        },
                        // No running session: the process is past its last
                        // session, so the socket is as good as gone.
                        Some(None) | None => return,
                    }
                }
            },
        };
        if connection.send(&response).is_err() {
            return;
        }
    }
}

/// Carry `client_id`'s event stream and input on its own connection until the
/// peer goes away, then detach the client.
///
/// The connection is split: a spawned thread drains the client's queue and
/// writes one frame per delivery that says something about the session's
/// structure, while this thread reads the client's own frames. A `KeyPress`, a
/// `Resize`, a `Paste`, a `SubmitCommand` and a `Mouse` round all cross to the
/// dispatcher over the inbox, and this half writes nothing back for any of
/// them: the first four are answered by the next painted frame, and a `Mouse`
/// round is answered on the writing half by exactly one
/// [`SessionEvent::MouseAnswer`] carrying that round's `request_id`. A request
/// of any other kind, end of stream, a transport fault, or a dispatcher that is
/// gone all end the reading loop.
///
/// Either half ending detaches the client, which removes its record and drops
/// its subscription; the closed queue, or the terminal `Quit` frame, then
/// ends the writing thread. Both notify, so a write that fails while the
/// reading half is still reading is cleaned up too — a detach for a client
/// already gone changes nothing.
///
/// A detach the server starts closes the client's queue: the writing thread
/// drains what is already queued, writes [`SessionEvent::Detached`] as its
/// last frame, and ends. The reading half keeps reading until the client
/// closes its end, and that close reads as end of stream — a second detach for
/// a client already gone.
fn stream_events(
    connection: Connection,
    client_id: ClientId,
    events: Receiver<Delivery>,
    inbox_tx: &Sender<RuntimeEvent>,
) {
    let (mut reader, mut writer) = connection.split();
    let writer_inbox = inbox_tx.clone();
    std::thread::spawn(move || {
        loop {
            let Ok(delivery) = events.recv() else {
                // The queue closed, which the server does when it detaches this
                // client. `recv` hands back everything queued before the close,
                // so the goodbye follows the events that preceded it.
                let _ = writer.send(&SessionEvent::Detached);
                break;
            };
            if let Some(event) = wire_event(&delivery) {
                match writer.send(&event) {
                    Ok(()) => {}
                    // A frame over the cap is refused with nothing written, so
                    // the connection is whole and the next frame carries the
                    // session's picture again.
                    Err(IpcError::FrameTooLarge { len, max }) => {
                        tracing::warn!(%client_id, len, max, "frame over the cap was not sent");
                    }
                    Err(_) => break,
                }
                // `Quit` is the stream's terminal frame; the loop ends with
                // it rather than waiting for the queue to close.
                if matches!(event, SessionEvent::Quit) {
                    break;
                }
            }
        }
        let _ = writer_inbox.send(RuntimeEvent::ClientDetached { client_id });
    });

    while let Ok(request) = reader.recv::<IncomingRequest>() {
        let kind = match request.kind {
            MaybeKnown::Known(kind) => kind,
            // A kind this build does not have comes from a newer koshi. This
            // connection carries the client's typing, so the one request is
            // dropped and the client keeps its stream.
            MaybeKnown::Unknown { name } => {
                tracing::debug!(%client_id, %name, "request kind this build does not have");
                continue;
            }
        };
        let event = match kind {
            IpcRequestKind::KeyPress { chord } => RuntimeEvent::ClientKeyPress { client_id, chord },
            IpcRequestKind::Resize { viewport } => RuntimeEvent::Resize {
                client_id,
                size: viewport,
            },
            IpcRequestKind::Paste { text } => RuntimeEvent::HostPaste { client_id, text },
            IpcRequestKind::Mouse(actions) => RuntimeEvent::ClientMouse {
                client_id,
                request_id: request.request_id,
                actions,
            },
            IpcRequestKind::SubmitCommand(envelope) => {
                // The result is answered by the next painted frame, so the
                // reply channel's receiving end goes straight away and the
                // dispatcher's send into it fails harmlessly.
                let (reply, _) = mpsc::channel();
                RuntimeEvent::Ipc {
                    envelope: *envelope,
                    reply,
                }
            }
            IpcRequestKind::Hello { .. }
            | IpcRequestKind::Attach { .. }
            | IpcRequestKind::Discovery
            | IpcRequestKind::Layout { .. } => break,
        };
        if inbox_tx.send(event).is_err() {
            break;
        }
    }
    let _ = inbox_tx.send(RuntimeEvent::ClientDetached { client_id });
}

/// Hand one request to the dispatcher thread and wait for its answer: build
/// the inbox event around a fresh reply channel, send it, and block on the
/// reply. `None` means the dispatcher is gone — the process is tearing down —
/// so the caller closes its connection without an answer.
fn ask_dispatcher<T>(
    inbox_tx: &Sender<RuntimeEvent>,
    build_event: impl FnOnce(mpsc::Sender<T>) -> RuntimeEvent,
) -> Option<T> {
    let (reply_tx, reply_rx) = mpsc::channel();
    if inbox_tx.send(build_event(reply_tx)).is_err() {
        return None;
    }
    reply_rx.recv().ok()
}

#[cfg(test)]
mod tests;
