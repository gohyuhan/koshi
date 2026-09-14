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
//! the session ends, or it has had no link for `SUPERVISOR_IDLE_EXIT_DURATION`, which
//! is 30 seconds. Either way it closes every pane it still holds before it
//! goes.

use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{ExitStatus, KillPolicy};
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::supervisor::{
    compute_supervisor_socket_address, IncomingSupervisorRequest, SupervisorEvent,
    SupervisorHandshake, SupervisorMessage, SupervisorPane, SupervisorRequestKind,
    SupervisorResponse, SupervisorResult,
};
use koshi_ipc::transport::{Connection, FrameReader, FrameWriter, Listener};
use koshi_ipc::wire::MaybeKnown;
use koshi_pty::backend::state::{PtyBackend, PtySink};
use koshi_pty::portable::PortablePtyBackend;

#[cfg(test)]
mod tests;

/// The subcommand koshi starts itself under to hold one session's panes. The
/// arguments after it are the session id, the link token, and
/// [`RUNTIME_DIRECTORY_FLAG`](koshi_link::router_client::RUNTIME_DIRECTORY_FLAG) with the
/// directory the session serves.
pub const PTY_SUPERVISOR_SUBCOMMAND: &str = "serve-pty-supervisor";

/// How long the supervisor waits for a link. A window that passes with no link
/// closes every pane it holds and ends the process.
///
/// Longer than the wait a session server coming up from carried state spends
/// on the link.
pub(crate) const SUPERVISOR_IDLE_EXIT_DURATION: Duration = Duration::from_secs(30);

/// How long the accept loop pauses after a failed accept before trying again.
const ACCEPT_RETRY_DELAY_DURATION: Duration = Duration::from_millis(100);

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
    link_state: Mutex<LinkState>,
    /// Wakes every parked send when the link or either list changes.
    changed: Condvar,
}

/// The link, and the answers that hold a send back or let it give up.
struct LinkState {
    /// The writing half of the link every frame goes out on, and `None` while
    /// no session server is linked.
    frame_writer: Option<FrameWriter>,
    /// Whether a Hello has opened the link. An event waits until the link is
    /// open, so a peer that has not presented the token is handed no pane output.
    is_open: bool,
    /// Whether the linked session server asked for pane events to be held. An
    /// event waits while it is set, so a session server about to replace its
    /// own process image is written nothing more.
    ///
    /// [`PauseOutput`](SupervisorRequestKind::PauseOutput) sets it;
    /// [`ResumeOutput`](SupervisorRequestKind::ResumeOutput) and the next link
    /// clear it.
    is_paused: bool,
    /// Every pane this supervisor has started closing. A send for one of these
    /// gives up, so the pane's reader reaches the end of its terminal and the
    /// close can finish, and no chunk of a closed pane reaches a link after the
    /// [`Kill`](SupervisorRequestKind::Kill) that closed it was answered.
    ///
    /// A pane id is never reused, so this only grows: one entry per pane the
    /// supervisor closes.
    closing_pane_ids: HashSet<PaneId>,
    /// The panes whose exit has been written to a link and whose entry the
    /// backend still holds. A pane leaves the backend on a
    /// [`Kill`](SupervisorRequestKind::Kill), and these are the ones no session
    /// server sent that Kill for; [`close_panes_that_ended`] closes them.
    ended_pane_ids: HashSet<PaneId>,
    /// Whether the supervisor is ending. Every parked send gives up.
    is_closing: bool,
}

impl LinkSink {
    /// A sink with no link yet.
    fn new() -> Arc<LinkSink> {
        Arc::new(LinkSink {
            link_state: Mutex::new(LinkState {
                frame_writer: None,
                is_open: false,
                is_paused: false,
                closing_pane_ids: HashSet::new(),
                ended_pane_ids: HashSet::new(),
                is_closing: false,
            }),
            changed: Condvar::new(),
        })
    }

    /// Store `frame_writer` as the link every response now uses. Events keep
    /// waiting until [`mark_link_open`](Self::mark_link_open) reports an
    /// accepted Hello, and a hold requested by the previous link is lifted.
    fn set_link_writer(&self, frame_writer: FrameWriter) {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        link_state.frame_writer = Some(frame_writer);
        link_state.is_open = false;
        link_state.is_paused = false;
    }

    /// Report the Hello that opened the link, releasing every parked send onto
    /// it.
    fn mark_link_open(&self) {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        link_state.is_open = true;
        drop(link_state);
        self.changed.notify_all();
    }

    /// Hold every pane event here instead of writing it to the link: a send
    /// already parked stays parked, and the next one parks too.
    ///
    /// The response to the request that calls this is written after it returns,
    /// and no pane event follows it on this link.
    fn pause_event_output(&self) {
        self.link_state.lock().expect("supervisor link").is_paused = true;
    }

    /// Write pane events to the link again, releasing every send parked by
    /// [`pause_event_output`](Self::pause_event_output) onto it.
    fn resume_event_output(&self) {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        link_state.is_paused = false;
        drop(link_state);
        self.changed.notify_all();
    }

    /// Drop the link, so every send parks until the next one arrives.
    fn clear_link(&self) {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        link_state.frame_writer = None;
        link_state.is_open = false;
    }

    /// Let `pane_id` go: a send parked for it gives up at once, and so does the
    /// next one.
    ///
    /// Called before a pane is closed, and never undone: every subsequent send for
    /// that pane gives up too. Closing a pane's terminal waits for its reader
    /// to carry that terminal to the end.
    fn mark_pane_closing(&self, pane_id: PaneId) {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        link_state.closing_pane_ids.insert(pane_id);
        drop(link_state);
        self.changed.notify_all();
    }

    /// Forget a pane that is now closed: a pane the backend no longer holds has
    /// no entry left to close.
    ///
    /// The pane stays in `closing_pane_ids`, so a reader still parked in
    /// [`send_event`](Self::send_event) gives its chunk up when it wakes rather than
    /// writing it to a link after the close was answered.
    fn remove_ended_pane(&self, pane_id: PaneId) {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        link_state.ended_pane_ids.remove(&pane_id);
    }

    /// The panes whose exit has been written to a link and whose entry the
    /// backend still holds.
    fn list_ended_pane_ids(&self) -> Vec<PaneId> {
        self.link_state
            .lock()
            .expect("supervisor link")
            .ended_pane_ids
            .iter()
            .copied()
            .collect()
    }

    /// End every send: each parked one gives up at once, and so does every
    /// send after it. Called before the supervisor closes its panes, the way
    /// [`mark_pane_closing`](Self::mark_pane_closing) is called before one pane is closed.
    fn mark_closing(&self) {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        link_state.is_closing = true;
        drop(link_state);
        self.changed.notify_all();
    }

    /// Send one response on the link. `false` means the link broke, which the
    /// caller reads as the end of that link.
    ///
    /// This never parks. A response that finds no link is dropped, not held for
    /// the next one.
    fn send_response(&self, supervisor_response: SupervisorResponse) -> bool {
        let supervisor_message =
            SupervisorMessage::<_, SupervisorEvent>::Response(supervisor_response);
        let mut link_state = self.link_state.lock().expect("supervisor link");
        let Some(frame_writer) = link_state.frame_writer.as_mut() else {
            return false;
        };
        if frame_writer.send(&supervisor_message).is_err() {
            link_state.frame_writer = None;
            return false;
        }
        true
    }

    /// Send one pane's event, waiting when no link is open and while the linked
    /// session server holds the output.
    ///
    /// `false` means nobody will ever want it: the pane is closed or being
    /// closed, or the supervisor is ending.
    fn send_event(&self, pane_id: PaneId, supervisor_event: SupervisorEvent) -> bool {
        let supervisor_message = SupervisorMessage::<SupervisorResult, _>::Event(supervisor_event);
        let mut link_state = self.link_state.lock().expect("supervisor link");
        loop {
            if link_state.is_closing || link_state.closing_pane_ids.contains(&pane_id) {
                return false;
            }
            if link_state.is_open && !link_state.is_paused {
                if let Some(frame_writer) = link_state.frame_writer.as_mut() {
                    if frame_writer.send(&supervisor_message).is_ok() {
                        return true;
                    }
                    link_state.frame_writer = None;
                }
            }
            link_state = self.changed.wait(link_state).expect("supervisor link");
        }
    }
}

impl PtySink for LinkSink {
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        self.send_event(
            pane_id,
            SupervisorEvent::Output {
                pane_id,
                output_bytes,
            },
        )
    }

    /// Send one pane's exit, and record the pane once that frame is on a link.
    ///
    /// [`close_panes_that_ended`] reads that record to close the pane's entry. A
    /// pane is recorded only after its exit is written.
    fn accept_exit_status(&self, pane_id: PaneId, exit_status: ExitStatus) {
        if self.send_event(
            pane_id,
            SupervisorEvent::Exited {
                pane_id,
                exit_status,
            },
        ) {
            self.link_state
                .lock()
                .expect("supervisor link")
                .ended_pane_ids
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

/// Keep `session_id`'s panes alive until the session ends.
///
/// Binds the supervisor link at [`compute_supervisor_socket_address`], under this
/// process's own id, and serves one linked session server at a time, opening
/// and closing every pane here. `connection_token` is the secret a link presents
/// at Hello: the session server generated it and started this process with it.
///
/// Returns once the session server asks the supervisor to end, or once it has
/// had no link for `SUPERVISOR_IDLE_EXIT_DURATION`, which is 30 seconds. Either way
/// every pane it still holds is closed first.
///
/// # Errors
/// Returns [`IpcError`] when the link address cannot be bound.
pub fn run_pty_supervisor(
    runtime_directory: &Path,
    session_id: SessionId,
    connection_token: ConnectionToken,
) -> Result<(), IpcError> {
    let supervisor_socket_address =
        compute_supervisor_socket_address(runtime_directory, session_id, std::process::id());
    let supervisor_listener = Listener::bind(&supervisor_socket_address)?;
    run_pane_supervisor_loop(
        supervisor_listener,
        &connection_token,
        SUPERVISOR_IDLE_EXIT_DURATION,
    );
    // On Unix the address is a socket file, which stays on disk after the
    // listener is dropped.
    koshi_ipc::endpoint::remove_socket_file(&supervisor_socket_address);
    Ok(())
}

/// Open every pane on one backend owned by this process, serve one link at a
/// time on `supervisor_listener`, and close every pane before returning.
///
/// Returns once a link asks the supervisor to end, or once it has had no link
/// for `idle_exit_duration` — whether or not it holds panes, so a session server that
/// dies before it ever links leaves no pane child running.
fn run_pane_supervisor_loop(
    supervisor_listener: Listener,
    connection_token: &ConnectionToken,
    idle_exit_duration: Duration,
) {
    let link_sink = LinkSink::new();
    let pty_backend = PortablePtyBackend::with_pty_sink(Arc::clone(&link_sink) as Arc<dyn PtySink>);

    let (connection_sender, connection_receiver) = channel::<Connection>();
    start_accept_thread(supervisor_listener, connection_sender);

    loop {
        let Ok(supervisor_connection) = connection_receiver.recv_timeout(idle_exit_duration) else {
            break;
        };
        let (frame_reader, frame_writer) = supervisor_connection.split();
        link_sink.set_link_writer(frame_writer);
        let link_outcome =
            serve_supervisor_link(frame_reader, &link_sink, &pty_backend, connection_token);
        link_sink.clear_link();
        if link_outcome == LinkOutcome::ShutdownRequested {
            break;
        }
    }

    close_every_pane(&link_sink, &pty_backend);
}

/// Close every pane the supervisor still holds: no child and no terminal is
/// left behind.
///
/// Every send is ended first. Closing a pane's terminal waits for its reader to
/// carry that terminal to the end.
fn close_every_pane(link_sink: &LinkSink, pty_backend: &PortablePtyBackend) {
    link_sink.mark_closing();
    for carried_pane in pty_backend.list_carried_panes() {
        let _ = pty_backend.kill_pane(carried_pane.pane_id, KillPolicy::Tree);
    }
}

/// Start the thread that accepts links.
///
/// A failed accept pauses for [`ACCEPT_RETRY_DELAY_DURATION`] and retries. The thread
/// ends when the main loop drops its receiver, which is the supervisor exiting.
fn start_accept_thread(supervisor_listener: Listener, connection_sender: Sender<Connection>) {
    let _ = std::thread::Builder::new()
        .name("koshi-pty-accept".to_string())
        .spawn(move || loop {
            match supervisor_listener.accept() {
                Ok(supervisor_connection) => {
                    // Only the user who started this supervisor may drive its
                    // panes. The operating system reports which user opened the
                    // link; a peer cannot claim to be another one.
                    if !matches!(supervisor_connection.is_peer_same_user(), Ok(true)) {
                        continue;
                    }
                    if connection_sender.send(supervisor_connection).is_err() {
                        return;
                    }
                }
                Err(_) => std::thread::sleep(ACCEPT_RETRY_DELAY_DURATION),
            }
        });
}

/// How long one link has to present the Hello that opens its gate, counted
/// from the moment the link is served.
///
/// The supervisor serves one link at a time, so a peer that connects and sends
/// nothing holds every subsequent link out for no longer than this.
const HANDSHAKE_WINDOW_DURATION: Duration = Duration::from_secs(10);

/// Serve one link until its peer hangs up, a fault closes it, or the session
/// server asks the supervisor to end.
///
/// A [`SupervisorHandshake`] gates every request. A malformed-but-aligned frame
/// is answered with [`IpcErrorCode::MalformedRequest`], and a request kind this
/// build does not have is refused by name; the link keeps serving after either.
///
/// Reads before the gate opens end at [`HANDSHAKE_WINDOW_DURATION`]; a link that
/// presents no accepted Hello inside it is [`LinkOutcome::Broken`], and the
/// next link is served. Reads after the gate opens have no deadline.
fn serve_supervisor_link(
    mut frame_reader: FrameReader,
    link_sink: &LinkSink,
    pty_backend: &PortablePtyBackend,
    connection_token: &ConnectionToken,
) -> LinkOutcome {
    let mut handshake = SupervisorHandshake::from_connection_token(connection_token.clone());
    frame_reader.set_deadline(Some(Instant::now() + HANDSHAKE_WINDOW_DURATION));
    loop {
        let supervisor_request: IncomingSupervisorRequest = match frame_reader.recv() {
            Ok(supervisor_request) => supervisor_request,
            Err(IpcError::MalformedFrame { .. }) => {
                // The frame was read whole: the stream is still aligned and
                // only its bytes were unreadable. `request_id: None` tells the
                // peer the response belongs to no request of its own.
                let refusal_response = SupervisorResponse {
                    request_id: None,
                    answer_result: SupervisorResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::MalformedRequest,
                        message: "the bytes received are not a request this build can read"
                            .to_string(),
                    }),
                };
                if link_sink.send_response(refusal_response) {
                    continue;
                }
                return LinkOutcome::Broken;
            }
            // An oversize frame's payload was never read: the stream's framing
            // is lost. Disconnects and transport faults have no stream left.
            // All close this one link.
            Err(_) => return LinkOutcome::Broken,
        };

        let request_id = Some(supervisor_request.request_id);
        let request_kind = match supervisor_request.request_kind {
            MaybeKnown::Known(request_kind) => request_kind,
            MaybeKnown::Unknown {
                variant_name: unknown_variant_name,
            } => {
                let refusal_response = SupervisorResponse {
                    request_id,
                    answer_result: SupervisorResult::Error(
                        handshake.build_unknown_request_kind_error(&unknown_variant_name),
                    ),
                };
                if link_sink.send_response(refusal_response) {
                    continue;
                }
                return LinkOutcome::Broken;
            }
        };

        let is_shutdown_request = request_kind == SupervisorRequestKind::Shutdown;
        let is_hello_request = matches!(request_kind, SupervisorRequestKind::Hello { .. });
        let response_result = match handshake.validate_request_kind(&request_kind) {
            Err(refusal_error) => SupervisorResult::Error(refusal_error),
            Ok(()) => {
                // An accepted Hello lets this link carry pane output. No output
                // reaches a peer that never presented the token, and the
                // handshake deadline comes off once it did.
                if is_hello_request {
                    link_sink.mark_link_open();
                    frame_reader.set_deadline(None);
                }
                serve_supervisor_request(link_sink, pty_backend, &handshake, request_kind)
            }
        };
        // A refused Shutdown ends nothing: the link keeps serving.
        let should_shutdown =
            is_shutdown_request && !matches!(response_result, SupervisorResult::Error(_));
        if !link_sink.send_response(SupervisorResponse {
            request_id,
            answer_result: response_result,
        }) {
            return LinkOutcome::Broken;
        }
        if should_shutdown {
            return LinkOutcome::ShutdownRequested;
        }
    }
}

/// Carry out one request the gate accepted, and build the response to send back.
fn serve_supervisor_request(
    link_sink: &LinkSink,
    pty_backend: &PortablePtyBackend,
    handshake: &SupervisorHandshake,
    request_kind: SupervisorRequestKind,
) -> SupervisorResult {
    match request_kind {
        SupervisorRequestKind::Hello { .. } => SupervisorResult::Hello {
            protocol_version: handshake
                .get_agreed_protocol_version()
                .expect("an accepted Hello settles the link's version"),
        },
        SupervisorRequestKind::Spawn {
            pane_id,
            spawn_spec,
            pty_size,
        } => match pty_backend.spawn_pane(pane_id, spawn_spec, pty_size) {
            // The handle carries no channels: this backend delivers through the
            // sink, and the pane's own record holds the process id.
            Ok(_pty_handle) => match pty_backend.get_child_process_id(pane_id) {
                Some(process_id) => SupervisorResult::Spawned { process_id },
                None => {
                    build_refused_result(format!("pane {pane_id} opened but reports no process id"))
                }
            },
            Err(pty_error) => build_refused_result(pty_error.to_string()),
        },
        SupervisorRequestKind::Resize { pane_id, pty_size } => {
            build_done_or_refused_result(pty_backend.resize_pane(pane_id, pty_size))
        }
        SupervisorRequestKind::Write {
            pane_id,
            input_bytes,
        } => build_done_or_refused_result(pty_backend.write_pane_input(pane_id, &input_bytes)),
        SupervisorRequestKind::Kill {
            pane_id,
            kill_policy,
        } => build_done_or_refused_result(close_pane(link_sink, pty_backend, pane_id, kill_policy)),
        SupervisorRequestKind::LiveCwd { pane_id } => SupervisorResult::Cwd(
            pty_backend
                .find_live_working_directory(pane_id)
                .filter(|working_directory_path| working_directory_path.to_str().is_some()),
        ),
        SupervisorRequestKind::ListPanes => {
            close_panes_that_ended(link_sink, pty_backend);
            SupervisorResult::Panes(
                pty_backend
                    .list_carried_panes()
                    .into_iter()
                    .map(|carried_pane| SupervisorPane {
                        pane_id: carried_pane.pane_id,
                        process_id: carried_pane.process_id,
                        pty_size: carried_pane.pty_size,
                    })
                    .collect(),
            )
        }
        SupervisorRequestKind::PauseOutput => {
            link_sink.pause_event_output();
            SupervisorResult::Done
        }
        SupervisorRequestKind::ResumeOutput => {
            link_sink.resume_event_output();
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
/// makes the response to [`ListPanes`](SupervisorRequestKind::ListPanes) name the
/// panes that are still running, and the session server linking now reports the
/// pane it carried as ended.
///
/// Every send for the pane is let go first, exactly as one
/// [`Kill`](SupervisorRequestKind::Kill) does.
fn close_panes_that_ended(link_sink: &LinkSink, pty_backend: &PortablePtyBackend) {
    for pane_id in link_sink.list_ended_pane_ids() {
        // The child already exited: `Force` signals nothing. The close drops
        // the pane's writer, joins its finished watcher, and frees its terminal.
        let _ = close_pane(link_sink, pty_backend, pane_id, KillPolicy::Force);
    }
}

/// Close one pane: let every send for it go, end its child under `kill_policy`,
/// then remove_ended_pane it.
///
/// The sends go first. Closing a pane's terminal waits for its reader to carry
/// that terminal to the end, and a reader parked in a send is not reading.
///
/// # Errors
/// Returns the failure of a pane the backend could not close.
fn close_pane(
    link_sink: &LinkSink,
    pty_backend: &PortablePtyBackend,
    pane_id: PaneId,
    kill_policy: KillPolicy,
) -> Result<(), koshi_pty::error::PtyError> {
    link_sink.mark_pane_closing(pane_id);
    let kill_result = pty_backend.kill_pane(pane_id, kill_policy);
    link_sink.remove_ended_pane(pane_id);
    kill_result
}

/// A refusal carrying `refusal_message`, under [`IpcErrorCode::Unknown`] — the code for
/// a pane failure.
fn build_refused_result(refusal_message: String) -> SupervisorResult {
    SupervisorResult::Error(IpcErrorPayload {
        code: IpcErrorCode::Unknown,
        message: refusal_message,
    })
}

/// [`Done`](SupervisorResult::Done) for a pane call that worked, and
/// [`build_refused_result`] carrying that call's failure otherwise.
fn build_done_or_refused_result(
    pty_operation_result: Result<(), koshi_pty::error::PtyError>,
) -> SupervisorResult {
    match pty_operation_result {
        Ok(()) => SupervisorResult::Done,
        Err(pty_error) => build_refused_result(pty_error.to_string()),
    }
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
/// `connection_token` is the secret the session server will present at Hello; it reaches
/// the supervisor on the command line and nowhere else.
///
/// # Errors
/// Returns the [`std::io::Error`] of a supervisor that could not be started,
/// with nothing started. The caller reports it as the pane failing to open.
#[cfg(windows)]
pub(crate) fn spawn_pty_supervisor(
    runtime_directory: &Path,
    session_id: SessionId,
    connection_token: &ConnectionToken,
) -> std::io::Result<u32> {
    crate::process::configure_detached_process(&mut std::process::Command::new(
        std::env::current_exe()?,
    ))
    .arg(PTY_SUPERVISOR_SUBCOMMAND)
    .arg(session_id.to_string())
    .arg(connection_token.expose())
    .arg(koshi_link::router_client::RUNTIME_DIRECTORY_FLAG)
    .arg(runtime_directory)
    .spawn()
    .map(|child_process| child_process.id())
}
