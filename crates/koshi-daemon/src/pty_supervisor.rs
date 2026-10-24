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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use koshi_config::layer::PartialKoshiConfig;
use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{ExitStatus, KillPolicy};
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::supervisor::{
    build_pane_output_frame_head, compute_supervisor_socket_address, IncomingSupervisorRequest,
    SupervisorEvent, SupervisorHandshake, SupervisorMessage, SupervisorPane, SupervisorRequestKind,
    SupervisorResponse, SupervisorResult, OUTPUT_WINDOW_PROTOCOL_VERSION,
    PANE_OUTPUT_WINDOW_BYTE_COUNT,
};
use koshi_ipc::transport::{Connection, FrameReader, FrameWriter, Listener};
use koshi_ipc::wire::MaybeKnown;
use koshi_observability::logging::{initialize_tracing, LogFileOwner};
use koshi_observability::repeated_warning::WarningRateLimiter;
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
/// terminal and blocks.
///
/// On a link of [`OUTPUT_WINDOW_PROTOCOL_VERSION`] or newer, each pane also
/// has a credit, which starts at [`PANE_OUTPUT_WINDOW_BYTE_COUNT`] on each new
/// link. A chunk goes out as binary frames of at most the pane's credit, each
/// frame spends its bytes, and a send with no credit left waits for the
/// session server's [`GrantPaneOutput`](SupervisorRequestKind::GrantPaneOutput).
/// On an older link, each chunk goes out whole as one JSON frame.
///
/// Example — a pane with 10 bytes of credit sends a 25-byte chunk as a
/// 10-byte frame, waits, and sends the other 15 bytes once grants cover them.
///
/// The part of a chunk in flight is written whole to the link that takes it:
/// a swap loses no byte. A frame half written to a link that broke goes with
/// that link, and that part is re-sent once.
///
/// A parked send gives up in two cases: the pane is being closed, or the
/// supervisor is ending.
struct LinkSink {
    /// The link and what may give up on it.
    link_state: Mutex<LinkState>,
    /// Wakes every parked send when the link opens, the output is resumed, a
    /// pane gets output credit, a pane starts closing, or the supervisor
    /// starts ending.
    link_state_changed: Condvar,
}

/// The link, and the answers that hold a send back or let it give up.
struct LinkState {
    /// The writing half of the link every frame goes out on, and `None` while
    /// no session server is linked.
    frame_writer: Option<FrameWriter>,
    /// The number of links this supervisor has taken, the current one
    /// included. A response written off the serve loop names the link its
    /// request arrived on, and is dropped once a newer link replaced that one.
    link_generation: u64,
    /// The protocol version the Hello that opened the link settled on, and
    /// `None` until a Hello opens it. An event waits until the link is open: a
    /// peer that has not presented the token is handed no pane output.
    link_protocol_version: Option<u32>,
    /// Each pane's output credit on this link, on a link of
    /// [`OUTPUT_WINDOW_PROTOCOL_VERSION`] or newer. A pane with no entry has
    /// [`PANE_OUTPUT_WINDOW_BYTE_COUNT`]. A new link clears every entry, and
    /// closing a pane removes its entry.
    output_credit_by_pane_id: HashMap<PaneId, u32>,
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
                link_generation: 0,
                link_protocol_version: None,
                output_credit_by_pane_id: HashMap::new(),
                is_paused: false,
                closing_pane_ids: HashSet::new(),
                ended_pane_ids: HashSet::new(),
                is_closing: false,
            }),
            link_state_changed: Condvar::new(),
        })
    }

    /// Store `frame_writer` as the link every response now uses, and return
    /// that link's generation. Events keep waiting until
    /// [`mark_link_open`](Self::mark_link_open) reports an accepted Hello, a
    /// hold requested by the previous link is lifted, and every pane's output
    /// credit goes back to [`PANE_OUTPUT_WINDOW_BYTE_COUNT`].
    fn set_link_writer(&self, frame_writer: FrameWriter) -> u64 {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        link_state.frame_writer = Some(frame_writer);
        link_state.link_generation += 1;
        link_state.link_protocol_version = None;
        link_state.is_paused = false;
        link_state.output_credit_by_pane_id.clear();
        link_state.link_generation
    }

    /// Report the Hello that opened the link at `link_protocol_version`,
    /// releasing every parked send onto it.
    fn mark_link_open(&self, link_protocol_version: u32) {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        link_state.link_protocol_version = Some(link_protocol_version);
        drop(link_state);
        self.link_state_changed.notify_all();
    }

    /// Add `granted_byte_count` to `pane_id`'s output credit on this link, up
    /// to [`PANE_OUTPUT_WINDOW_BYTE_COUNT`], and release a send parked for
    /// credit. A grant for a pane being closed changes nothing.
    fn grant_pane_output(&self, pane_id: PaneId, granted_byte_count: u32) {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        if link_state.closing_pane_ids.contains(&pane_id) {
            return;
        }
        let output_credit = link_state
            .output_credit_by_pane_id
            .entry(pane_id)
            .or_insert(PANE_OUTPUT_WINDOW_BYTE_COUNT);
        *output_credit = output_credit
            .saturating_add(granted_byte_count)
            .min(PANE_OUTPUT_WINDOW_BYTE_COUNT);
        drop(link_state);
        self.link_state_changed.notify_all();
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
        self.link_state_changed.notify_all();
    }

    /// Drop the link, so every send parks until the next one arrives.
    fn clear_link(&self) {
        let mut link_state = self.link_state.lock().expect("supervisor link");
        link_state.frame_writer = None;
        link_state.link_protocol_version = None;
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
        link_state.output_credit_by_pane_id.remove(&pane_id);
        drop(link_state);
        self.link_state_changed.notify_all();
    }

    /// Forget a pane that is now closed: a pane the backend no longer holds has
    /// no entry left to close.
    ///
    /// The pane stays in `closing_pane_ids`: a reader still parked in
    /// [`send_output`](Self::send_output) gives its chunk up when it wakes, and
    /// writes nothing to a link after the close was answered.
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
        self.link_state_changed.notify_all();
    }

    /// Send one response on the link of generation `link_generation`, the
    /// one its request arrived on. `false` means that link broke or is no
    /// longer the current one, which the serve loop reads as the end of that
    /// link.
    ///
    /// This never parks. A response that finds no link, or a newer link, is
    /// dropped, not held for the next one.
    fn send_response(&self, link_generation: u64, supervisor_response: SupervisorResponse) -> bool {
        let supervisor_message =
            SupervisorMessage::<_, SupervisorEvent>::Response(supervisor_response);
        let mut link_state = self.link_state.lock().expect("supervisor link");
        if link_state.link_generation != link_generation {
            return false;
        }
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
            if link_state.link_protocol_version.is_some() && !link_state.is_paused {
                if let Some(frame_writer) = link_state.frame_writer.as_mut() {
                    if frame_writer.send(&supervisor_message).is_ok() {
                        return true;
                    }
                    link_state.frame_writer = None;
                }
            }
            link_state = self
                .link_state_changed
                .wait(link_state)
                .expect("supervisor link");
        }
    }

    /// Send one chunk of `pane_id`'s output, waiting when no link is open,
    /// while the linked session server holds the output, and, on a link of
    /// [`OUTPUT_WINDOW_PROTOCOL_VERSION`] or newer, while the pane has no
    /// output credit. The chunk goes out in parts
    /// ([`write_output_part`](Self::write_output_part)) until every byte is
    /// sent. An empty chunk sends nothing.
    ///
    /// `false` means nobody will ever want it: the pane is closed or being
    /// closed, or the supervisor is ending. The parts sent before that stay
    /// sent.
    fn send_output(&self, pane_id: PaneId, output_bytes: &[u8]) -> bool {
        let mut unsent_output_bytes = output_bytes;
        let mut link_state = self.link_state.lock().expect("supervisor link");
        while !unsent_output_bytes.is_empty() {
            if link_state.is_closing || link_state.closing_pane_ids.contains(&pane_id) {
                return false;
            }
            match Self::write_output_part(&mut link_state, pane_id, unsent_output_bytes) {
                Some(sent_byte_count) => {
                    unsent_output_bytes = &unsent_output_bytes[sent_byte_count..];
                }
                None => {
                    link_state = self
                        .link_state_changed
                        .wait(link_state)
                        .expect("supervisor link")
                }
            }
        }
        true
    }

    /// Write the next part of `unsent_output_bytes`, `pane_id`'s output, to
    /// the link `link_state` holds, and return that part's byte count.
    ///
    /// On a link of [`OUTPUT_WINDOW_PROTOCOL_VERSION`] or newer, the part is
    /// at most the pane's output credit, goes out as one binary frame
    /// ([`build_pane_output_frame_head`]), and spends its byte count from that
    /// credit. On an older link, the part is every unsent byte, as one JSON
    /// [`SupervisorEvent::Output`].
    ///
    /// `None` when no part goes out now: no link is open, the output is held,
    /// the pane has no credit, or the write failed, which drops the link.
    fn write_output_part(
        link_state: &mut LinkState,
        pane_id: PaneId,
        unsent_output_bytes: &[u8],
    ) -> Option<usize> {
        let link_protocol_version = link_state.link_protocol_version?;
        if link_state.is_paused {
            return None;
        }
        let frame_writer = link_state.frame_writer.as_mut()?;
        let write_result = if link_protocol_version < OUTPUT_WINDOW_PROTOCOL_VERSION {
            let supervisor_message =
                SupervisorMessage::<SupervisorResult, _>::Event(SupervisorEvent::Output {
                    pane_id,
                    output_bytes: unsent_output_bytes.to_vec(),
                });
            frame_writer
                .send(&supervisor_message)
                .map(|()| unsent_output_bytes.len())
        } else {
            let output_credit = link_state
                .output_credit_by_pane_id
                .get(&pane_id)
                .copied()
                .unwrap_or(PANE_OUTPUT_WINDOW_BYTE_COUNT);
            let part_byte_count = unsent_output_bytes.len().min(output_credit as usize);
            if part_byte_count == 0 {
                return None;
            }
            frame_writer
                .send_binary_with_head(
                    &build_pane_output_frame_head(pane_id),
                    &unsent_output_bytes[..part_byte_count],
                )
                .map(|()| {
                    link_state
                        .output_credit_by_pane_id
                        .insert(pane_id, output_credit - part_byte_count as u32);
                    part_byte_count
                })
        };
        match write_result {
            Ok(sent_byte_count) => Some(sent_byte_count),
            Err(_) => {
                link_state.frame_writer = None;
                None
            }
        }
    }
}

impl PtySink for LinkSink {
    fn accept_output_bytes(&self, pane_id: PaneId, output_bytes: Vec<u8>) -> bool {
        self.send_output(pane_id, &output_bytes)
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
/// First installs the tracing subscriber from `app_config_layer`'s `logging`
/// section: with logging on, every line this process writes goes to the
/// session's own log file, `koshi-log-<uuid>.log`. `app_config_layer` is the
/// `koshi.kdl` layer the caller read at this process's start, and `None` keeps
/// logging off.
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
    app_config_layer: Option<&PartialKoshiConfig>,
    session_id: SessionId,
    connection_token: ConnectionToken,
) -> Result<(), IpcError> {
    let _ = initialize_tracing(koshi_link::config::build_logging_parameters(
        app_config_layer,
        LogFileOwner::Session { session_id },
    ));
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
    koshi_ipc::endpoint::delete_socket_file(&supervisor_socket_address);
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
    let pty_backend = Arc::new(PortablePtyBackend::with_pty_sink(
        Arc::clone(&link_sink) as Arc<dyn PtySink>
    ));
    let mut pane_kill_threads: Vec<JoinHandle<()>> = Vec::new();

    let (connection_sender, connection_receiver) = channel::<Connection>();
    start_accept_thread(supervisor_listener, connection_sender);

    loop {
        let Ok(supervisor_connection) = connection_receiver.recv_timeout(idle_exit_duration) else {
            break;
        };
        let (frame_reader, frame_writer) = supervisor_connection.split();
        let link_generation = link_sink.set_link_writer(frame_writer);
        let link_outcome = serve_supervisor_link(
            frame_reader,
            &SupervisorLink {
                link_sink: Arc::clone(&link_sink),
                pty_backend: Arc::clone(&pty_backend),
                link_generation,
            },
            connection_token,
            &mut pane_kill_threads,
        );
        link_sink.clear_link();
        if link_outcome == LinkOutcome::ShutdownRequested {
            break;
        }
    }

    close_every_pane(&link_sink, &pty_backend, pane_kill_threads);
}

/// Close every pane the supervisor still holds: no child and no terminal is
/// left behind.
///
/// 1. Every send is ended. Closing a pane's terminal waits for its reader to
///    carry that terminal to the end.
/// 2. Every kill still running on its own thread is waited for.
/// 3. Every pane left is closed with [`KillPolicy::Tree`].
fn close_every_pane(
    link_sink: &LinkSink,
    pty_backend: &PortablePtyBackend,
    pane_kill_threads: Vec<JoinHandle<()>>,
) {
    link_sink.mark_closing();
    for pane_kill_thread in pane_kill_threads {
        let _ = pane_kill_thread.join();
    }
    for carried_pane in pty_backend.list_carried_panes() {
        let _ = pty_backend.kill_pane(carried_pane.pane_id, KillPolicy::Tree);
    }
}

/// What serving one link reaches: the sink every frame goes out through, the
/// backend that holds the panes, and the link's generation.
struct SupervisorLink {
    /// The sink that writes every frame to the current link.
    link_sink: Arc<LinkSink>,
    /// The backend that opens and closes every pane.
    pty_backend: Arc<PortablePtyBackend>,
    /// The generation [`LinkSink::set_link_writer`] gave this link.
    link_generation: u64,
}

/// Start the thread that accepts links.
///
/// A failed accept is logged at warn level at most once inside
/// [`LOG_WINDOW_DURATION`](koshi_observability::repeated_warning::LOG_WINDOW_DURATION),
/// pauses for [`ACCEPT_RETRY_DELAY_DURATION`] and retries. The thread
/// ends when the main loop drops its receiver, which is the supervisor exiting.
fn start_accept_thread(supervisor_listener: Listener, connection_sender: Sender<Connection>) {
    let mut accept_failure_warning = WarningRateLimiter::new();
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
                Err(accept_error) => {
                    if accept_failure_warning.record_warning_if_due(Instant::now()) {
                        tracing::warn!(
                            %accept_error,
                            "the supervisor could not accept a link; \
                             retrying every {ACCEPT_RETRY_DELAY_DURATION:?}"
                        );
                    }
                    std::thread::sleep(ACCEPT_RETRY_DELAY_DURATION);
                }
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
/// A [`GrantPaneOutput`](SupervisorRequestKind::GrantPaneOutput) is answered
/// with nothing, a refusal included. An accepted
/// [`Kill`](SupervisorRequestKind::Kill) is answered by its own thread
/// ([`start_pane_kill`]), which is pushed onto `pane_kill_threads`, and the
/// loop reads the next request at once.
///
/// Reads before the gate opens end at [`HANDSHAKE_WINDOW_DURATION`]; a link that
/// presents no accepted Hello inside it is [`LinkOutcome::Broken`], and the
/// next link is served. Reads after the gate opens have no deadline.
fn serve_supervisor_link(
    mut frame_reader: FrameReader,
    supervisor_link: &SupervisorLink,
    connection_token: &ConnectionToken,
    pane_kill_threads: &mut Vec<JoinHandle<()>>,
) -> LinkOutcome {
    let link_sink = supervisor_link.link_sink.as_ref();
    let link_generation = supervisor_link.link_generation;
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
                if link_sink.send_response(link_generation, refusal_response) {
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
                if link_sink.send_response(link_generation, refusal_response) {
                    continue;
                }
                return LinkOutcome::Broken;
            }
        };

        let is_shutdown_request = request_kind == SupervisorRequestKind::Shutdown;
        let is_hello_request = matches!(request_kind, SupervisorRequestKind::Hello { .. });
        let is_grant_request =
            matches!(request_kind, SupervisorRequestKind::GrantPaneOutput { .. });
        let response_result = match handshake.validate_request_kind(&request_kind) {
            // A grant is answered with nothing, a refusal included.
            Err(_) if is_grant_request => continue,
            Err(refusal_error) => SupervisorResult::Error(refusal_error),
            Ok(()) => {
                // An accepted Hello lets this link carry pane output. No output
                // reaches a peer that never presented the token, and the
                // handshake deadline comes off once it did.
                if is_hello_request {
                    link_sink.mark_link_open(
                        handshake
                            .get_agreed_protocol_version()
                            .expect("an accepted Hello settles the link's version"),
                    );
                    frame_reader.set_deadline(None);
                }
                match serve_supervisor_request(
                    supervisor_link,
                    &handshake,
                    request_id,
                    request_kind,
                    pane_kill_threads,
                ) {
                    Some(response_result) => response_result,
                    None => continue,
                }
            }
        };
        // A refused Shutdown ends nothing: the link keeps serving.
        let should_shutdown =
            is_shutdown_request && !matches!(response_result, SupervisorResult::Error(_));
        if !link_sink.send_response(
            link_generation,
            SupervisorResponse {
                request_id,
                answer_result: response_result,
            },
        ) {
            return LinkOutcome::Broken;
        }
        if should_shutdown {
            return LinkOutcome::ShutdownRequested;
        }
    }
}

/// Carry out one request the gate accepted, and build the response the serve
/// loop sends back.
///
/// `None` for a request the serve loop does not answer: a
/// [`GrantPaneOutput`](SupervisorRequestKind::GrantPaneOutput), which nothing
/// answers, and a [`Kill`](SupervisorRequestKind::Kill), which
/// [`start_pane_kill`] answers to `request_id` from its own thread.
fn serve_supervisor_request(
    supervisor_link: &SupervisorLink,
    handshake: &SupervisorHandshake,
    request_id: Option<u64>,
    request_kind: SupervisorRequestKind,
    pane_kill_threads: &mut Vec<JoinHandle<()>>,
) -> Option<SupervisorResult> {
    let link_sink = supervisor_link.link_sink.as_ref();
    let pty_backend = supervisor_link.pty_backend.as_ref();
    let supervisor_result = match request_kind {
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
            // The backend delivers through the sink, and the pane's own
            // record holds the process id.
            Ok(()) => match pty_backend.get_child_process_id(pane_id) {
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
        } => {
            start_pane_kill(
                supervisor_link,
                request_id,
                pane_id,
                kill_policy,
                pane_kill_threads,
            );
            return None;
        }
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
        SupervisorRequestKind::GrantPaneOutput {
            pane_id,
            granted_byte_count,
        } => {
            link_sink.grant_pane_output(pane_id, granted_byte_count);
            return None;
        }
    };
    Some(supervisor_result)
}

/// Close `pane_id` under `kill_policy` ([`close_pane`]) on a thread of its
/// own, and answer request `request_id` from that thread once the close
/// returns: [`Done`](SupervisorResult::Done), or the close's failure as a
/// refusal. The answer goes out only while the link the request arrived on is
/// still the current one.
///
/// The thread joins `pane_kill_threads`, after the threads that already
/// ended leave it. When the operating system starts no thread, the close and
/// its answer run on this thread.
///
/// Example — a graceful Kill with a 3-second window, then a Resize: the
/// Resize is answered at once, and the Kill about 3 seconds after it arrived.
fn start_pane_kill(
    supervisor_link: &SupervisorLink,
    request_id: Option<u64>,
    pane_id: PaneId,
    kill_policy: KillPolicy,
    pane_kill_threads: &mut Vec<JoinHandle<()>>,
) {
    pane_kill_threads.retain(|pane_kill_thread| !pane_kill_thread.is_finished());
    let link_sink = Arc::clone(&supervisor_link.link_sink);
    let pty_backend = Arc::clone(&supervisor_link.pty_backend);
    let link_generation = supervisor_link.link_generation;
    match std::thread::Builder::new()
        .name("koshi-pty-kill".to_string())
        .spawn(move || {
            close_pane_and_answer(
                &link_sink,
                &pty_backend,
                link_generation,
                request_id,
                pane_id,
                kill_policy,
            );
        }) {
        Ok(pane_kill_thread) => pane_kill_threads.push(pane_kill_thread),
        Err(_) => close_pane_and_answer(
            &supervisor_link.link_sink,
            &supervisor_link.pty_backend,
            link_generation,
            request_id,
            pane_id,
            kill_policy,
        ),
    }
}

/// Close `pane_id` under `kill_policy` ([`close_pane`]), then answer request
/// `request_id` on the link of generation `link_generation`:
/// [`Done`](SupervisorResult::Done), or the close's failure as a refusal.
fn close_pane_and_answer(
    link_sink: &LinkSink,
    pty_backend: &PortablePtyBackend,
    link_generation: u64,
    request_id: Option<u64>,
    pane_id: PaneId,
    kill_policy: KillPolicy,
) {
    let close_result = close_pane(link_sink, pty_backend, pane_id, kill_policy);
    let _ = link_sink.send_response(
        link_generation,
        SupervisorResponse {
            request_id,
            answer_result: build_done_or_refused_result(close_result),
        },
    );
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
/// then drop it from the ended panes.
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

/// Start the supervisor that holds `session_id`'s panes, and hand back its
/// process id once it is running.
///
/// It runs the koshi program file at the path
/// [`resolve_program_path`](koshi_host::program_path::resolve_program_path)
/// gives, under [`PTY_SUPERVISOR_SUBCOMMAND`], with no console of its own and
/// a process group of its own, and its input and output go nowhere. After an
/// update, that file can hold a newer koshi than this process runs.
///
/// The supervisor binds the link address that its own process id names. The
/// caller derives that address from the process id handed back.
///
/// `connection_token` is the secret the session server presents at Hello. It
/// reaches the supervisor on the command line only.
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
    koshi_host::detached_process::configure_detached_process(&mut std::process::Command::new(
        koshi_host::program_path::resolve_program_path()?,
    ))
    .arg(PTY_SUPERVISOR_SUBCOMMAND)
    .arg(session_id.to_string())
    .arg(connection_token.expose_secret())
    .arg(koshi_link::router_client::RUNTIME_DIRECTORY_FLAG)
    .arg(runtime_directory)
    .spawn()
    .map(|child_process| child_process.id())
}
