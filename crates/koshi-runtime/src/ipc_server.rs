//! The control-socket server: how a running koshi answers its socket.
//!
//! [`IpcServer::start`] binds the session's control-socket address, writes
//! the endpoint file advertising it, and spawns the accept loop. Each
//! accepted connection gets its own thread holding its own
//! [`Handshake`] gate: a Hello must open the connection before any other
//! request is served. A `SubmitCommand` or `Discovery` request crosses to
//! the dispatcher thread through the runtime inbox with a reply channel;
//! the dispatcher's answer comes back on it and leaves as the connection's
//! response frame.
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
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use koshi_core::ids::SessionId;
use koshi_ipc::endpoint::{socket_addr, EndpointFile};
use koshi_ipc::error::IpcError;
use koshi_ipc::handshake::Handshake;
use koshi_ipc::protocol::{
    ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcRequest, IpcRequestKind, IpcResponse,
    IpcResult,
};
use koshi_ipc::transport::{Connection, Listener};
use koshi_ipc::validate::{reclaim_stale_socket, validate_socket_addr};

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
        // Teardown lives in `Drop`, so consuming `self` is the whole job;
        // the method exists so call sites read as intent.
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

/// Unlink the socket file at `addr` on Unix, where the address is a
/// filesystem path. On Windows the address is a pipe name that vanishes with
/// its last handle, so there is nothing to remove.
fn remove_socket_file(addr: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(addr);
    }
    #[cfg(windows)]
    {
        let _ = addr;
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
/// [`Handshake`] gates every request, `SubmitCommand` and `Discovery` cross
/// to the dispatcher over the inbox and answer with its reply, and a
/// malformed-but-aligned frame is answered with
/// [`IpcErrorCode::MalformedRequest`] while the connection keeps serving.
fn serve_connection(
    mut connection: Connection,
    token: ConnectionToken,
    inbox_tx: &Sender<RuntimeEvent>,
) {
    let mut gate = Handshake::new(token);
    loop {
        let request: IpcRequest = match connection.recv() {
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
        let response = match gate.check(&request.kind) {
            Err(refusal) => IpcResponse {
                request_id,
                result: IpcResult::Error(refusal),
            },
            Ok(()) => match request.kind {
                IpcRequestKind::Hello { .. } => IpcResponse {
                    request_id,
                    result: IpcResult::Hello,
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
            },
        };
        if connection.send(&response).is_err() {
            return;
        }
    }
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
