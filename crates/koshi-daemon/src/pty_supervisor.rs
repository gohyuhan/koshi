//! The process that holds one session's panes.
//!
//! A pseudoconsole — what stands behind a pane's terminal on Windows — cannot
//! be handed to another process. Its handle is a private allocation, and the
//! only documented way a pseudoconsole reaches another process is as a client
//! at `CreateProcess` time. Exiting closes the handles that keep its console
//! host alive, which terminates every attached client and its process tree, so
//! the process that opens a pseudoconsole must never exit while its panes are
//! meant to live.
//!
//! This process owns the real
//! [`PortablePtyBackend`](koshi_pty::portable::PortablePtyBackend), so every
//! pane's terminal is opened and closed here and nowhere else, and it outlives
//! the session server that started it. The session server drives it over a
//! link and reads every byte its panes print back over that same link. The link
//! may break and come back, which is what a session server replacing its own
//! image looks like from here.
//!
//! It ends in one of two ways: the session server sends
//! [`Shutdown`](koshi_ipc::supervisor::SupervisorRequestKind::Shutdown) when
//! the session ends, or it has had no link for
//! [`SUPERVISOR_IDLE_EXIT`](crate::pty_supervisor::SUPERVISOR_IDLE_EXIT).
//! Either way it closes every pane it still holds before it goes.

use std::collections::HashSet;
use std::path::Path;
#[cfg(windows)]
use std::process::Stdio;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{ExitStatus, KillPolicy};
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::supervisor::{
    supervisor_socket_addr, IncomingSupervisorRequest, SupervisorEvent, SupervisorHandshake,
    SupervisorMessage, SupervisorPane, SupervisorRequestKind, SupervisorResponse, SupervisorResult,
};
use koshi_ipc::transport::{Connection, FrameReader, FrameWriter, Listener};
use koshi_ipc::wire::MaybeKnown;
use koshi_pty::backend::state::{PtyBackend, PtySink};
use koshi_pty::portable::PortablePtyBackend;

#[cfg(test)]
mod tests;

/// The subcommand koshi starts itself under to hold one session's panes. The
/// arguments after it are the session id, the link token, and
/// [`RUNTIME_DIR_FLAG`](koshi_link::router_client::RUNTIME_DIR_FLAG) with the
/// directory the session serves.
pub const PTY_SUPERVISOR_SUBCOMMAND: &str = "serve-pty-supervisor";

/// How long the supervisor waits for a link. A window that passes with no link
/// closes every pane it holds and ends the process.
///
/// Longer than the wait a session server coming up from carried state spends
/// on the link, so a supervisor never gives up on an image still trying to
/// reach it.
pub const SUPERVISOR_IDLE_EXIT: Duration = Duration::from_secs(30);

/// How long the accept loop pauses after a failed accept before trying again,
/// so a persistent accept error cannot spin a core.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Where one pane's output and exit go: out over the link, one frame each.
///
/// A send holds the chunk it is carrying until the write succeeds. While no
/// link is up, and while the linked session server holds the output, it waits,
/// and the pane's own terminal applies the backpressure: the child fills its
/// terminal and blocks. The one chunk in flight is written whole to the link
/// that takes it, so a swap loses no byte.
///
/// A frame half written to a link that broke goes with that link, so the chunk
/// is re-sent once, not twice.
///
/// A parked send gives up in two cases: the pane is being closed, or the
/// supervisor is ending.
struct LinkSink {
    /// The link and what may give up on it.
    state: Mutex<LinkState>,
    /// Wakes every parked send when the link or either list changes.
    changed: Condvar,
}

/// The link, and the answers that hold a send back or let it give up.
struct LinkState {
    /// The writing half of the link every frame goes out on, and `None` while
    /// no session server is linked.
    writer: Option<FrameWriter>,
    /// Whether a Hello has opened the link. An event waits until one has, so a
    /// peer that has not presented the token is handed no pane output.
    opened: bool,
    /// Whether the linked session server asked for pane events to be held. An
    /// event waits while it is set, so a session server about to replace its
    /// own process image is written nothing more.
    ///
    /// [`PauseOutput`](SupervisorRequestKind::PauseOutput) sets it;
    /// [`ResumeOutput`](SupervisorRequestKind::ResumeOutput) and the next link
    /// clear it.
    paused: bool,
    /// The panes being closed right now. A send parked for one of these gives
    /// up, so the pane's reader reaches the end of its terminal and the close
    /// can finish.
    letting_go: HashSet<PaneId>,
    /// The panes whose exit has been written to a link and whose entry the
    /// backend still holds. A pane leaves the backend on a
    /// [`Kill`](SupervisorRequestKind::Kill), and these are the ones no session
    /// server sent that Kill for; [`close_panes_that_ended`] closes them.
    ended: HashSet<PaneId>,
    /// Whether the supervisor is ending. Every parked send gives up.
    closing: bool,
}

impl LinkSink {
    /// A sink with no link yet.
    fn new() -> Arc<LinkSink> {
        Arc::new(LinkSink {
            state: Mutex::new(LinkState {
                writer: None,
                opened: false,
                paused: false,
                letting_go: HashSet::new(),
                ended: HashSet::new(),
                closing: false,
            }),
            changed: Condvar::new(),
        })
    }

    /// Take `writer` as the link every answer now goes out on. Events keep
    /// waiting until [`open`](Self::open) reports an accepted Hello, and a hold
    /// the link before this one asked for is lifted: a hold belongs to the link
    /// that asked for it.
    fn link_up(&self, writer: FrameWriter) {
        let mut state = self.state.lock().expect("supervisor link");
        state.writer = Some(writer);
        state.opened = false;
        state.paused = false;
    }

    /// Report the Hello that opened the link, releasing every parked send onto
    /// it.
    fn open(&self) {
        let mut state = self.state.lock().expect("supervisor link");
        state.opened = true;
        drop(state);
        self.changed.notify_all();
    }

    /// Hold every pane event here instead of writing it to the link: a send
    /// already parked stays parked, and the next one parks too.
    ///
    /// The answer to the request that calls this is written after it returns,
    /// and no pane event follows it on this link.
    fn pause_output(&self) {
        self.state.lock().expect("supervisor link").paused = true;
    }

    /// Write pane events to the link again, releasing every send parked by
    /// [`pause_output`](Self::pause_output) onto it.
    fn resume_output(&self) {
        let mut state = self.state.lock().expect("supervisor link");
        state.paused = false;
        drop(state);
        self.changed.notify_all();
    }

    /// Drop the link, so every send parks until the next one arrives.
    fn link_down(&self) {
        let mut state = self.state.lock().expect("supervisor link");
        state.writer = None;
        state.opened = false;
    }

    /// Let `pane` go: a send parked for it gives up at once, and so does the
    /// next one.
    ///
    /// Called before a pane is closed. Closing a pane's terminal waits for its
    /// reader to carry that terminal to the end, so a reader parked in a send is
    /// released first.
    fn let_go(&self, pane: PaneId) {
        let mut state = self.state.lock().expect("supervisor link");
        state.letting_go.insert(pane);
        drop(state);
        self.changed.notify_all();
    }

    /// Forget a pane that is now closed: the panes being closed are only the
    /// ones being closed right now, and a pane the backend no longer holds has
    /// no entry left to close.
    fn forget(&self, pane: PaneId) {
        let mut state = self.state.lock().expect("supervisor link");
        state.letting_go.remove(&pane);
        state.ended.remove(&pane);
    }

    /// The panes whose exit has been written to a link and whose entry the
    /// backend still holds.
    fn ended_panes(&self) -> Vec<PaneId> {
        self.state
            .lock()
            .expect("supervisor link")
            .ended
            .iter()
            .copied()
            .collect()
    }

    /// End every send: each parked one gives up at once, and so does every
    /// send after it. Called before the supervisor closes its panes, the way
    /// [`let_go`](Self::let_go) is called before one pane is closed.
    fn close(&self) {
        let mut state = self.state.lock().expect("supervisor link");
        state.closing = true;
        drop(state);
        self.changed.notify_all();
    }

    /// Send one answer on the link. `false` means the link broke, which the
    /// caller reads as the end of that link.
    ///
    /// An answer belongs to the link its request arrived on, so this never
    /// parks.
    fn answer(&self, response: SupervisorResponse) -> bool {
        let frame = SupervisorMessage::<_, SupervisorEvent>::Response(response);
        let mut state = self.state.lock().expect("supervisor link");
        let Some(writer) = state.writer.as_mut() else {
            return false;
        };
        if writer.send(&frame).is_err() {
            state.writer = None;
            return false;
        }
        true
    }

    /// Send one pane's event, waiting when no link is open and while the linked
    /// session server holds the output.
    ///
    /// `false` means nobody will ever want it: the pane is being closed, or
    /// the supervisor is ending.
    fn forward(&self, pane: PaneId, event: SupervisorEvent) -> bool {
        let frame = SupervisorMessage::<SupervisorResult, _>::Event(event);
        let mut state = self.state.lock().expect("supervisor link");
        loop {
            if state.closing || state.letting_go.contains(&pane) {
                return false;
            }
            if state.opened && !state.paused {
                if let Some(writer) = state.writer.as_mut() {
                    if writer.send(&frame).is_ok() {
                        return true;
                    }
                    state.writer = None;
                }
            }
            state = self.changed.wait(state).expect("supervisor link");
        }
    }
}

impl PtySink for LinkSink {
    fn output(&self, pane_id: PaneId, bytes: Vec<u8>) -> bool {
        self.forward(pane_id, SupervisorEvent::Output { pane_id, bytes })
    }

    /// Send one pane's exit, and record the pane once that frame is on a link.
    ///
    /// [`close_panes_that_ended`] reads that record to close the pane's entry. A
    /// pane is recorded only after its exit is written, so letting it go never
    /// cancels a send still waiting for a link.
    fn exit(&self, pane_id: PaneId, status: ExitStatus) {
        if self.forward(pane_id, SupervisorEvent::Exited { pane_id, status }) {
            self.state
                .lock()
                .expect("supervisor link")
                .ended
                .insert(pane_id);
        }
    }
}

/// Why one link ended.
#[derive(Debug, PartialEq, Eq)]
enum LinkOutcome {
    /// The peer hung up or a fault closed the link. The supervisor keeps its
    /// panes and waits for the next one.
    Broken,
    /// The session server asked the supervisor to end.
    ShutdownRequested,
}

/// Hold `session_id`'s panes until the session ends.
///
/// Binds the supervisor link at [`supervisor_socket_addr`], under this
/// process's own id, and serves one linked session server at a time, opening
/// and closing every pane here. `token` is the secret a link presents at Hello:
/// the session server generated it and started this process with it.
///
/// Returns once the session server asks the supervisor to end, or once it has
/// had no link for [`SUPERVISOR_IDLE_EXIT`]. Either way every pane it still
/// holds is closed first.
///
/// # Errors
/// Returns [`IpcError`] when the link address cannot be bound.
pub fn run_pty_supervisor(
    runtime_dir: &Path,
    session_id: SessionId,
    token: ConnectionToken,
) -> Result<(), IpcError> {
    let addr = supervisor_socket_addr(runtime_dir, session_id, std::process::id());
    let listener = Listener::bind(&addr)?;
    hold_panes(listener, &token, SUPERVISOR_IDLE_EXIT);
    // On Unix the address is a socket file, which stays on disk after the
    // listener is dropped.
    koshi_ipc::endpoint::remove_socket_file(&addr);
    Ok(())
}

/// Open every pane on one backend of this process's own, serve one link at a
/// time on `listener`, and close every pane before returning.
///
/// Returns once a link asks the supervisor to end, or once it has had no link
/// for `idle_exit` — whether or not it holds panes, so a session server that
/// dies before it ever links leaves no pane child running.
fn hold_panes(listener: Listener, token: &ConnectionToken, idle_exit: Duration) {
    let sink = LinkSink::new();
    let backend = PortablePtyBackend::with_sink(Arc::clone(&sink) as Arc<dyn PtySink>);

    let (links_tx, links) = channel::<Connection>();
    start_accept_thread(listener, links_tx);

    loop {
        let Ok(connection) = links.recv_timeout(idle_exit) else {
            break;
        };
        let (reader, writer) = connection.split();
        sink.link_up(writer);
        let outcome = serve_link(reader, &sink, &backend, token);
        sink.link_down();
        if outcome == LinkOutcome::ShutdownRequested {
            break;
        }
    }

    close_every_pane(&sink, &backend);
}

/// Close every pane the supervisor still holds, so no child and no terminal is
/// left behind.
///
/// Every send is ended first: closing a pane's terminal waits for its reader to
/// carry that terminal to the end, so a reader parked in a send is released
/// before the close can finish.
fn close_every_pane(sink: &Arc<LinkSink>, backend: &PortablePtyBackend) {
    sink.close();
    for pane in backend.carried_panes() {
        let _ = backend.kill(pane.pane_id, KillPolicy::Tree);
    }
}

/// Start the thread that accepts links.
///
/// A failed accept pauses briefly and retries. The thread ends when the main
/// loop drops its receiver, which is the supervisor exiting.
fn start_accept_thread(listener: Listener, links_tx: Sender<Connection>) {
    let _ = std::thread::Builder::new()
        .name("koshi-pty-accept".to_string())
        .spawn(move || loop {
            match listener.accept() {
                Ok(connection) => {
                    // The operating system reports which user opened the link,
                    // so a peer cannot claim to be another one. Only the user
                    // who started this supervisor may drive its panes.
                    if !matches!(connection.peer_is_same_user(), Ok(true)) {
                        continue;
                    }
                    if links_tx.send(connection).is_err() {
                        return;
                    }
                }
                Err(_) => std::thread::sleep(ACCEPT_RETRY_DELAY),
            }
        });
}

/// Serve one link until its peer hangs up, a fault closes it, or the session
/// server asks the supervisor to end.
///
/// A [`SupervisorHandshake`] gates every request. A malformed-but-aligned frame
/// is answered with [`IpcErrorCode::MalformedRequest`], and a request kind this
/// build does not have is refused by name; the link keeps serving after either.
fn serve_link(
    mut reader: FrameReader,
    sink: &Arc<LinkSink>,
    backend: &PortablePtyBackend,
    token: &ConnectionToken,
) -> LinkOutcome {
    let mut gate = SupervisorHandshake::new(token.clone());
    loop {
        let request: IncomingSupervisorRequest = match reader.recv() {
            Ok(request) => request,
            Err(IpcError::MalformedFrame { .. }) => {
                // The frame was read whole, so the stream is still aligned;
                // only its bytes were unreadable. `request_id: None` tells the
                // peer the answer belongs to no request of its own.
                let refusal = SupervisorResponse {
                    request_id: None,
                    result: SupervisorResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::MalformedRequest,
                        message: "the bytes received are not a request this build can read"
                            .to_string(),
                    }),
                };
                if sink.answer(refusal) {
                    continue;
                }
                return LinkOutcome::Broken;
            }
            // An oversize frame's payload was never read, so the stream's
            // framing is lost; disconnects and transport faults have no stream
            // left. All close this one link.
            Err(_) => return LinkOutcome::Broken,
        };

        let request_id = Some(request.request_id);
        let kind = match request.kind {
            MaybeKnown::Known(kind) => kind,
            MaybeKnown::Unknown { name } => {
                let refusal = SupervisorResponse {
                    request_id,
                    result: SupervisorResult::Error(gate.refuse_unknown(&name)),
                };
                if sink.answer(refusal) {
                    continue;
                }
                return LinkOutcome::Broken;
            }
        };

        let ending = kind == SupervisorRequestKind::Shutdown;
        let opening = matches!(kind, SupervisorRequestKind::Hello { .. });
        let result = match gate.check(&kind) {
            Err(refusal) => SupervisorResult::Error(refusal),
            Ok(()) => {
                // An accepted Hello is what lets this link carry pane output, so
                // no output reaches a peer that never presented the token.
                if opening {
                    sink.open();
                }
                serve_request(sink, backend, &gate, kind)
            }
        };
        // A refused Shutdown ends nothing: the link keeps serving.
        let ending = ending && !matches!(result, SupervisorResult::Error(_));
        if !sink.answer(SupervisorResponse { request_id, result }) {
            return LinkOutcome::Broken;
        }
        if ending {
            return LinkOutcome::ShutdownRequested;
        }
    }
}

/// Carry out one request the gate accepted, and build the answer to send back.
fn serve_request(
    sink: &Arc<LinkSink>,
    backend: &PortablePtyBackend,
    gate: &SupervisorHandshake,
    kind: SupervisorRequestKind,
) -> SupervisorResult {
    match kind {
        SupervisorRequestKind::Hello { .. } => SupervisorResult::Hello {
            protocol_version: gate
                .agreed()
                .expect("an accepted Hello settles the link's version"),
        },
        SupervisorRequestKind::Spawn {
            pane_id,
            spec,
            size,
        } => match backend.spawn(pane_id, spec, size) {
            // The handle carries no channels: this backend delivers through the
            // sink, and the pane's own record holds the process id.
            Ok(_handle) => match child_pid(backend, pane_id) {
                Some(pid) => SupervisorResult::Spawned { pid },
                None => refused(format!("pane {pane_id} opened but reports no process id")),
            },
            Err(error) => refused(error.to_string()),
        },
        SupervisorRequestKind::Resize { pane_id, size } => match backend.resize(pane_id, size) {
            Ok(()) => SupervisorResult::Done,
            Err(error) => refused(error.to_string()),
        },
        SupervisorRequestKind::Write { pane_id, bytes } => match backend.write(pane_id, &bytes) {
            Ok(()) => SupervisorResult::Done,
            Err(error) => refused(error.to_string()),
        },
        SupervisorRequestKind::Kill {
            pane_id,
            kill_policy,
        } => match close_pane(sink, backend, pane_id, kill_policy) {
            Ok(()) => SupervisorResult::Done,
            Err(error) => refused(error.to_string()),
        },
        SupervisorRequestKind::LiveCwd { pane_id } => {
            SupervisorResult::Cwd(backend.live_cwd(pane_id))
        }
        SupervisorRequestKind::ListPanes => {
            close_panes_that_ended(sink, backend);
            SupervisorResult::Panes(
                backend
                    .carried_panes()
                    .into_iter()
                    .map(|pane| SupervisorPane {
                        pane_id: pane.pane_id,
                        pid: pane.pid,
                        size: pane.size,
                    })
                    .collect(),
            )
        }
        SupervisorRequestKind::PauseOutput => {
            sink.pause_output();
            SupervisorResult::Done
        }
        SupervisorRequestKind::ResumeOutput => {
            sink.resume_output();
            SupervisorResult::Done
        }
        SupervisorRequestKind::Shutdown => SupervisorResult::Done,
    }
}

/// Close every pane whose exit has already been written to a link.
///
/// A pane leaves the backend on a [`Kill`](SupervisorRequestKind::Kill), which a
/// session server sends once it applies the exit. A session server that replaces
/// its own process image between the two never sends it. Closing the pane here
/// makes the answer to [`ListPanes`](SupervisorRequestKind::ListPanes) name the
/// panes that are still running, and the session server linking now reports the
/// pane it carried as ended.
///
/// Every send for the pane is let go first, exactly as one
/// [`Kill`](SupervisorRequestKind::Kill) does.
fn close_panes_that_ended(sink: &Arc<LinkSink>, backend: &PortablePtyBackend) {
    for pane in sink.ended_panes() {
        // The child already exited, so `Force` signals nothing: the close drops
        // the pane's writer, joins its finished watcher, and frees its terminal.
        let _ = close_pane(sink, backend, pane, KillPolicy::Force);
    }
}

/// Close one pane: let every send for it go, end its child under `kill_policy`,
/// then forget it.
///
/// The sends go first. Closing a pane's terminal waits for its reader to carry
/// that terminal to the end, and a reader parked in a send is not reading.
///
/// # Errors
/// Returns the failure of a pane the backend could not close.
fn close_pane(
    sink: &Arc<LinkSink>,
    backend: &PortablePtyBackend,
    pane: PaneId,
    kill_policy: KillPolicy,
) -> Result<(), koshi_pty::error::PtyError> {
    sink.let_go(pane);
    let killed = backend.kill(pane, kill_policy);
    sink.forget(pane);
    killed
}

/// The process id of `pane`'s child, or `None` when the supervisor does not
/// hold that pane.
fn child_pid(backend: &PortablePtyBackend, pane: PaneId) -> Option<u32> {
    backend
        .carried_panes()
        .into_iter()
        .find(|held| held.pane_id == pane)
        .map(|held| held.pid)
}

/// A refusal carrying `message`, under [`IpcErrorCode::Unknown`] — the code for
/// a pane failure.
fn refused(message: String) -> SupervisorResult {
    SupervisorResult::Error(IpcErrorPayload {
        code: IpcErrorCode::Unknown,
        message,
    })
}

/// Start the supervisor that will hold `session_id`'s panes, and hand back its
/// process id once it is running.
///
/// It runs the binary this process runs, under
/// [`PTY_SUPERVISOR_SUBCOMMAND`], with no console of its own and a process
/// group of its own, and its input and output go nowhere.
///
/// The process id is what the caller derives the supervisor's link address
/// from, since the supervisor binds the address its own id names.
///
/// `token` is the secret the session server will present at Hello; it reaches
/// the supervisor on the command line and nowhere else.
///
/// # Errors
/// Returns the [`std::io::Error`] of a supervisor that could not be started,
/// with nothing started. The caller reports it as the pane failing to open.
#[cfg(windows)]
pub fn spawn_pty_supervisor(
    runtime_dir: &Path,
    session_id: SessionId,
    token: &ConnectionToken,
) -> std::io::Result<u32> {
    use std::os::windows::process::CommandExt;

    std::process::Command::new(std::env::current_exe()?)
        .arg(PTY_SUPERVISOR_SUBCOMMAND)
        .arg(session_id.to_string())
        .arg(token.expose())
        .arg(koshi_link::router_client::RUNTIME_DIR_FLAG)
        .arg(runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(crate::router::DETACHED_PROCESS | crate::router::CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(|child| child.id())
}
