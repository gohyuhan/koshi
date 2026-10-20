//! Driving panes that live in another process.
//!
//! A pseudoconsole (the Windows kernel object behind a pane's terminal) stays
//! with the process that opened it. A session server that replaces its own
//! image keeps its panes in a helper process, the supervisor, and drives them
//! over a link.
//!
//! [`SupervisorPtyBackend`](crate::supervisor::SupervisorPtyBackend) is that
//! link as a
//! [`PtyBackend`](crate::backend::state::PtyBackend): each call is one request
//! and its answer, and every byte a pane prints arrives on the link and is
//! handed to the [`PtySink`](crate::backend::state::PtySink) the backend was
//! built with. Under the same sink it behaves as
//! [`PortablePtyBackend`](crate::portable::PortablePtyBackend) does.
//!
//! Requests from several threads cross the link at once. Each request carries
//! its own id, and the link's reader thread hands each answer to the caller
//! whose request the answer names.
//!
//! The process at the other end speaks [`koshi_ipc::supervisor`]'s protocol;
//! koshi's own binary runs it under a hidden subcommand.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use koshi_core::ids::PaneId;
use koshi_core::process::{ExitStatus, KillPolicy, PtySize, SpawnSpec};
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::supervisor::migration::{
    serialize_previous_supervisor_request, PreviousSupervisorMessage,
    PREVIOUS_SUPERVISOR_PROTOCOL_VERSION,
};
use koshi_ipc::supervisor::{
    decode_pane_output_frame, IncomingSupervisorMessage, SupervisorEvent, SupervisorMessage,
    SupervisorRequest, SupervisorRequestKind, SupervisorResponse, SupervisorResult,
    MINIMUM_OUTPUT_GRANT_BYTE_COUNT, OUTPUT_WINDOW_PROTOCOL_VERSION, SUPERVISOR_PROTOCOL_VERSION,
};
use koshi_ipc::transport::{
    Connection, FrameReader, FrameWriter, ReceivedFrame, CONNECT_WAIT_DURATION,
};
use koshi_ipc::wire::{MaybeKnown, WireName};

use crate::backend::state::{CarriedPtyPane, PtyBackend, PtySink, UNOBSERVED_EXIT};
use crate::error::PtyError;

/// How long one request waits for its answer, counted from the moment the
/// request is written. A request not answered within this window is reported
/// as failed.
///
/// The supervisor serves one link at a time. The window includes the time it
/// is still spending on the link a replaced process image left behind. A
/// supervisor that speaks protocol 2 or lower answers one request at a time,
/// in the order the requests arrive, so the window also includes the requests
/// written before this one.
const ANSWER_WAIT_DURATION: Duration = Duration::from_secs(10);

/// What this side keeps for one pane the supervisor holds.
///
/// The supervisor is the authority on the pane itself.
/// [`list_carried_panes`](SupervisorPtyBackend::list_carried_panes) reads the
/// process id and size without a round trip.
#[derive(Debug, Clone, Copy)]
struct LivePane {
    /// The process id of the pane's child, as the supervisor reported it.
    process_id: u32,
    /// The last size this pane's terminal was set to: what it was spawned or
    /// taken back at, then whatever the newest successful
    /// [`resize_pane`](PtyBackend::resize_pane) carried.
    pty_size: PtySize,
    /// The bytes of this pane's output that the consumer parsed and that this
    /// side has not granted back to the supervisor yet. Grows only on a link
    /// of [`OUTPUT_WINDOW_PROTOCOL_VERSION`] or newer.
    ungranted_output_byte_count: usize,
}

/// What the callers of one backend and its link reader thread share, under one
/// lock.
struct LinkBook {
    /// The id the next request carries. The first request carries 1, and each
    /// request after it carries the next number.
    next_request_id: u64,
    /// Where the answer to each request still waiting for one goes, keyed by
    /// the request's id. A request that expects no answer has no entry.
    answer_sender_by_request_id: HashMap<u64, Sender<LinkAnswer>>,
    /// The panes this backend drives. Empty until
    /// [`connect`](SupervisorPtyBackend::connect) has reconciled the pane
    /// list. A spawn adds a pane, a kill removes one, and the link closing
    /// removes every one.
    live_pane_by_id: HashMap<PaneId, LivePane>,
    /// Exit statuses the link reader received before the pane list was
    /// reconciled. `None` once reconciliation took them.
    exit_status_by_pane_id_during_connect: Option<HashMap<PaneId, ExitStatus>>,
    /// Why the link closed, and `None` while it is open. Every request made
    /// after the link closed fails without being written.
    link_close_cause: Option<LinkCloseCause>,
}

/// Why the link reader stopped reading the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkCloseCause {
    /// The link broke, the supervisor hung up, or this side closed the
    /// link's read direction.
    ReadFailed,
    /// A frame from the supervisor did not parse at the protocol version the
    /// link speaks.
    UnreadableFrame,
}

/// What the link reader hands the caller waiting on one request.
enum LinkAnswer {
    /// The response that names the caller's request.
    Response(SupervisorResponse<MaybeKnown<SupervisorResult>>),
    /// A response that named no request this side sent, or named no request
    /// at all. Every caller waiting at that moment receives one. Carries the
    /// request id the response named.
    StrayResponse { answered_request_id: Option<u64> },
}

/// One request written to the link whose answer is still to be read.
struct SentRequest {
    /// The id the request carried.
    request_id: u64,
    /// The request's kind, by name, for error messages.
    request_kind_name: &'static str,
    /// When the request was written.
    written_at: Instant,
    /// How long after `written_at` the answer may arrive.
    answer_wait_duration: Duration,
    /// Where the link reader hands this request's answer.
    answer_receiver: Receiver<LinkAnswer>,
}

/// The link: its writing half, and the book its reader thread shares.
struct Link {
    /// The writing half: every request goes out here. The lock is held for
    /// one frame write, never while an answer is awaited.
    frame_writer: Mutex<FrameWriter>,
    /// The supervisor-link protocol version every frame on this link is
    /// written and read in: [`SUPERVISOR_PROTOCOL_VERSION`], or
    /// [`PREVIOUS_SUPERVISOR_PROTOCOL_VERSION`] for a supervisor that koshi
    /// 0.3.0 or 0.4.0 started.
    frame_protocol_version: u32,
    /// The requests waiting for answers and the panes this backend drives,
    /// shared with the link reader thread.
    link_book: Arc<Mutex<LinkBook>>,
}

/// Why one link did not open.
#[derive(Debug)]
enum LinkOpenError {
    /// The Hello failed after a frame from the supervisor did not parse at the
    /// protocol version the link spoke. Carries the failure the Hello reported.
    UnreadableHelloAnswer { hello_error: PtyError },
    /// Every other failure. `link_error` is that failure.
    OtherFailure { link_error: PtyError },
}

impl LinkOpenError {
    /// The failure this carries.
    fn into_pty_error(self) -> PtyError {
        match self {
            LinkOpenError::UnreadableHelloAnswer { hello_error } => hello_error,
            LinkOpenError::OtherFailure { link_error } => link_error,
        }
    }
}

/// A [`PtyBackend`] whose panes live in a supervisor process.
///
/// Every pane's pseudo-terminal is opened and closed by that process and by no
/// other. This backend's own process can exit and be replaced while every pane
/// keeps running. A replacement image calls [`connect`](Self::connect) again
/// and drives the same panes.
///
/// When the link closes after [`connect`](Self::connect) returned, because
/// the supervisor ended or sent a frame that does not parse:
///
/// 1. Every request waiting for an answer fails at once, and every request
///    sent after the close fails without being written.
/// 2. Every pane this backend still drives leaves it and is reported to the
///    sink as ended with [`UNOBSERVED_EXIT`]. A pane whose exit or refused
///    output the sink already received is not reported again.
///
/// On a link of [`OUTPUT_WINDOW_PROTOCOL_VERSION`] or newer,
/// [`grant_pane_output`](PtyBackend::grant_pane_output) grants each pane's
/// parsed output back to the supervisor, at least
/// [`MINIMUM_OUTPUT_GRANT_BYTE_COUNT`] bytes per grant.
///
/// One process builds one of these and keeps it. Dropping it leaves the link's
/// reader thread holding the reading half. The link closes when this process
/// exits or when [`shutdown_supervisor`](Self::shutdown_supervisor) ends the supervisor.
pub struct SupervisorPtyBackend {
    /// The link to the supervisor.
    link: Link,
    /// Whether the link gives each pane an output window: the Hello settled
    /// on [`OUTPUT_WINDOW_PROTOCOL_VERSION`] or newer.
    has_output_windows: bool,
    /// Exit statuses received before the pane list was reconciled.
    exit_status_by_pane_id_at_connect: Mutex<HashMap<PaneId, ExitStatus>>,
}

impl SupervisorPtyBackend {
    /// Open a link to the supervisor listening at `supervisor_address`, present
    /// `connection_token`, and reconcile which panes this backend drives.
    ///
    /// `pane_ids` is what the caller believes is running: empty for a session
    /// starting fresh, and the carried pane list for one that has just replaced
    /// its own image. It is settled against what the supervisor holds:
    ///
    /// - A pane the supervisor holds that is not in `pane_ids` is killed with
    ///   [`KillPolicy::Tree`], in the order the supervisor listed it. The
    ///   answer to that kill is not checked.
    /// - A pane in `pane_ids` the supervisor does not hold is reported to `pty_sink`
    ///   as ended, carrying `ExitCode(-1)` when no exit arrived on this link.
    ///   An `ExitCode(-1)` report follows the kills made during reconciliation.
    /// - A listed pane with a recorded exit is closed in the supervisor and
    ///   reported to `pty_sink` as ended.
    ///
    /// Every remaining pane is driven by the returned backend, at the process
    /// id and size the supervisor listed.
    ///
    /// The first link speaks [`SUPERVISOR_PROTOCOL_VERSION`]. When its Hello
    /// fails after a frame from the supervisor did not parse, that link is
    /// closed and a second link speaks [`PREVIOUS_SUPERVISOR_PROTOCOL_VERSION`],
    /// the version of a supervisor that koshi 0.3.0 or 0.4.0 started. Such a
    /// supervisor answers a Hello it cannot read with `MalformedRequest` and
    /// then serves the next link. Every other failure of the first link is the
    /// result, and no second link is opened.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the link cannot be opened, when another OS
    /// user serves `supervisor_address`, when the supervisor does not answer
    /// within the answer wait, when it refuses the Hello, pane list, or a
    /// recorded exit's kill, when it answers a request with the wrong response
    /// kind, or when the link closes before the pane list is reconciled. After
    /// a second link was opened, the failure is that link's. Any of those
    /// closes the link's read direction: the reader thread ends, and the
    /// supervisor serves the next link.
    ///
    /// # Panics
    /// Panics when the operating system cannot start the link's reader thread.
    pub fn connect(
        supervisor_address: &str,
        connection_token: ConnectionToken,
        pty_sink: Arc<dyn PtySink>,
        pane_ids: &[PaneId],
    ) -> Result<SupervisorPtyBackend, PtyError> {
        match Self::open_link(
            supervisor_address,
            connection_token.clone(),
            Arc::clone(&pty_sink),
            pane_ids,
            SUPERVISOR_PROTOCOL_VERSION,
        ) {
            Ok(backend) => return Ok(backend),
            Err(LinkOpenError::OtherFailure { link_error }) => return Err(link_error),
            Err(LinkOpenError::UnreadableHelloAnswer { .. }) => {}
        }
        Self::open_link(
            supervisor_address,
            connection_token,
            pty_sink,
            pane_ids,
            PREVIOUS_SUPERVISOR_PROTOCOL_VERSION,
        )
        .map_err(LinkOpenError::into_pty_error)
    }

    /// Open one link that writes and reads every frame in
    /// `frame_protocol_version`, present `connection_token` in its Hello,
    /// and reconcile `pane_ids` as [`connect`](Self::connect) states.
    ///
    /// A link whose Hello settles below [`OUTPUT_WINDOW_PROTOCOL_VERSION`] is
    /// logged once at info level.
    ///
    /// # Errors
    /// [`LinkOpenError::UnreadableHelloAnswer`] when the Hello fails after the
    /// link's reader met a frame that does not parse in
    /// `frame_protocol_version`. [`LinkOpenError::OtherFailure`] for every other
    /// failure [`connect`](Self::connect) names. Either way the link's read
    /// direction is closed.
    fn open_link(
        supervisor_address: &str,
        connection_token: ConnectionToken,
        pty_sink: Arc<dyn PtySink>,
        pane_ids: &[PaneId],
        frame_protocol_version: u32,
    ) -> Result<SupervisorPtyBackend, LinkOpenError> {
        let connection =
            Connection::connect_to_own_server_within(supervisor_address, CONNECT_WAIT_DURATION)
                .map_err(|connect_error| LinkOpenError::OtherFailure {
                    link_error: PtyError::Io {
                        detail: format!(
                            "the supervisor at {supervisor_address} could not be reached: \
                             {connect_error}"
                        ),
                    },
                })?;
        let link_closer = connection.create_read_closer().ok();
        let (frame_reader, frame_writer) = connection.split();
        let link_book = Arc::new(Mutex::new(LinkBook {
            next_request_id: 1,
            answer_sender_by_request_id: HashMap::new(),
            live_pane_by_id: HashMap::new(),
            exit_status_by_pane_id_during_connect: Some(HashMap::new()),
            link_close_cause: None,
        }));
        start_link_reader_thread(
            frame_reader,
            Arc::clone(&link_book),
            Arc::clone(&pty_sink),
            frame_protocol_version,
        );
        let link = Link {
            frame_writer: Mutex::new(frame_writer),
            frame_protocol_version,
            link_book,
        };

        let link_open_result = match link.send_hello_request(connection_token) {
            Err(hello_error)
                if link.lock_link_book().link_close_cause
                    == Some(LinkCloseCause::UnreadableFrame) =>
            {
                Err(LinkOpenError::UnreadableHelloAnswer { hello_error })
            }
            Err(hello_error) => Err(LinkOpenError::OtherFailure {
                link_error: hello_error,
            }),
            Ok(link_protocol_version) => link
                .reconcile_panes(pty_sink.as_ref(), pane_ids)
                .map(|exit_status_by_pane_id_at_connect| {
                    (link_protocol_version, exit_status_by_pane_id_at_connect)
                })
                .map_err(|reconcile_error| LinkOpenError::OtherFailure {
                    link_error: reconcile_error,
                }),
        };
        match link_open_result {
            Ok((link_protocol_version, exit_status_by_pane_id_at_connect)) => {
                let has_output_windows = link_protocol_version >= OUTPUT_WINDOW_PROTOCOL_VERSION;
                if !has_output_windows {
                    tracing::info!(
                        link_protocol_version,
                        "the process holding the panes speaks supervisor protocol \
                         {link_protocol_version}: pane output crosses its link with no \
                         output window"
                    );
                }
                Ok(SupervisorPtyBackend {
                    link,
                    has_output_windows,
                    exit_status_by_pane_id_at_connect: Mutex::new(
                        exit_status_by_pane_id_at_connect,
                    ),
                })
            }
            Err(link_open_error) => {
                if let Some(link_closer) = link_closer {
                    link_closer.close();
                }
                Err(link_open_error)
            }
        }
    }

    /// Hold every pane's reader still: nothing is read from a terminal without
    /// being handed to the consumer.
    ///
    /// Every pane's reader lives inside the supervisor, and the supervisor
    /// takes the hold: it stops writing pane events to this link, and requests
    /// and their answers keep crossing it. The link's one reader thread hands
    /// every frame to the sink before it reads the next. When this returns
    /// `Ok(())`, the consumer holds everything the supervisor wrote. What the
    /// panes print from then on waits inside the supervisor and reaches the
    /// next link.
    ///
    /// [`resume_readers`](Self::resume_readers) lifts the hold on this link,
    /// and a fresh link lifts it by opening.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the supervisor refuses the request, when
    /// it answers with something else, or when the link fails. A supervisor
    /// built before the request existed refuses it by name.
    pub fn pause_readers(&self) -> Result<(), PtyError> {
        self.link
            .send_request_and_require_done(SupervisorRequestKind::PauseOutput)
    }

    /// Put every held reader back to work: the supervisor writes what it held
    /// to this link and keeps writing.
    ///
    /// A refusal, a link that broke, and an answer that never came are all
    /// dropped here. Each one is a supervisor no longer serving this session,
    /// which every subsequent request on this link reports in turn.
    pub fn resume_readers(&self) {
        let _ = self
            .link
            .send_request_and_receive_result(SupervisorRequestKind::ResumeOutput);
    }

    /// Wait until no byte this backend took for a child is still queued.
    ///
    /// [`write_pane_input`](PtyBackend::write_pane_input) sends the bytes to the supervisor and
    /// waits for its answer. A write that has returned is already the supervisor's, and this
    /// process queues nothing. The supervisor keeps running across an image swap, and its own
    /// writer threads carry those bytes to the terminals.
    ///
    /// # Errors
    /// Never returns an error. The signature matches
    /// [`PortablePtyBackend::flush_writers`](crate::portable::PortablePtyBackend::flush_writers).
    pub fn flush_writers(&self) -> Result<(), PtyError> {
        Ok(())
    }

    /// One record per live pane, in no fixed order: what a new process image
    /// needs to take each pane back.
    ///
    /// The supervisor keeps holding the pane. The record carries the pane's
    /// identity, its child's process id and its size. The terminal descriptor
    /// is always `None`; that descriptor belongs to the supervisor. The exit
    /// status is always `None`; the supervisor reaps every child and reports
    /// the status over the link.
    pub fn list_carried_panes(&self) -> Vec<CarriedPtyPane> {
        self.link
            .lock_link_book()
            .live_pane_by_id
            .iter()
            .map(|(pane_id, live_pane)| CarriedPtyPane {
                pane_id: *pane_id,
                #[cfg(unix)]
                terminal_fd: None,
                process_id: live_pane.process_id,
                pty_size: live_pane.pty_size,
                exit_status: None,
            })
            .collect()
    }

    /// Take exit statuses sent before the pane list was reconciled.
    /// Each status names a pane absent from the live pane list.
    pub fn take_pane_exit_statuses_at_connect(&self) -> HashMap<PaneId, ExitStatus> {
        std::mem::take(
            &mut *self
                .exit_status_by_pane_id_at_connect
                .lock()
                .expect("supervisor exit statuses"),
        )
    }

    /// Tell the supervisor to close every pane it still holds and exit.
    ///
    /// The session server sends this when the session ends. A refusal, a link
    /// that broke, and an answer that did not arrive within the wait all read
    /// as success.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the supervisor answers Shutdown with
    /// something other than [`SupervisorResult::Done`].
    pub fn shutdown_supervisor(&self) -> Result<(), PtyError> {
        match self
            .link
            .send_request_and_receive_result(SupervisorRequestKind::Shutdown)
        {
            Ok(SupervisorResult::Done) => Ok(()),
            Ok(unexpected_result) => Err(build_unexpected_supervisor_result_error(
                "Shutdown",
                &unexpected_result,
            )),
            Err(_) => Ok(()),
        }
    }

    /// [`PtyError::UnknownPane`] when this backend does not drive `pane_id`.
    /// Checked before every request that names one pane.
    fn validate_pane_is_driven(&self, pane_id: PaneId) -> Result<(), PtyError> {
        if self
            .link
            .lock_link_book()
            .live_pane_by_id
            .contains_key(&pane_id)
        {
            Ok(())
        } else {
            Err(PtyError::UnknownPane { pane_id })
        }
    }

    /// Record `pty_size` as `pane_id`'s size, when this backend still drives
    /// `pane_id`.
    fn record_pane_size(&self, pane_id: PaneId, pty_size: PtySize) {
        if let Some(live_pane) = self.link.lock_link_book().live_pane_by_id.get_mut(&pane_id) {
            live_pane.pty_size = pty_size;
        }
    }
}

impl Link {
    /// The book this link shares with its reader thread, locked.
    ///
    /// # Panics
    /// When a thread panicked while it held the lock.
    fn lock_link_book(&self) -> MutexGuard<'_, LinkBook> {
        self.link_book.lock().expect("supervisor link book")
    }

    /// Present `connection_token` in a Hello and require the supervisor's Hello
    /// answer. Returns the protocol version the answer settles on.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the supervisor refuses the Hello, answers
    /// it with another response kind, or does not answer it.
    fn send_hello_request(&self, connection_token: ConnectionToken) -> Result<u32, PtyError> {
        match self.send_request_and_receive_result(SupervisorRequestKind::build_hello_request(
            connection_token,
        ))? {
            SupervisorResult::Hello { protocol_version } => Ok(protocol_version),
            unexpected_supervisor_result => Err(build_unexpected_supervisor_result_error(
                "Hello",
                &unexpected_supervisor_result,
            )),
        }
    }

    /// Read the pane list, and reconcile it against `pane_ids`, the panes the
    /// caller believes are running. Returns the exit statuses the link reader
    /// received before the pane list was reconciled.
    ///
    /// 1. A pane the supervisor holds that `pane_ids` does not name is killed
    ///    with [`KillPolicy::Tree`].
    /// 2. A listed pane with a recorded exit is killed with
    ///    [`KillPolicy::Force`].
    /// 3. A pane `pane_ids` names that the supervisor does not hold is
    ///    reported to `pty_sink` as ended if its exit has not arrived.
    /// 4. Every remaining pane is written into the link book's pane map.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the supervisor refuses the pane list or a
    /// recorded exit's kill, answers with the wrong response kind, does not
    /// answer, or when the link closed before step 4.
    fn reconcile_panes(
        &self,
        pty_sink: &dyn PtySink,
        pane_ids: &[PaneId],
    ) -> Result<HashMap<PaneId, ExitStatus>, PtyError> {
        let supervisor_panes =
            match self.send_request_and_receive_result(SupervisorRequestKind::ListPanes)? {
                SupervisorResult::Panes(supervisor_panes) => supervisor_panes,
                unexpected_supervisor_result => {
                    return Err(build_unexpected_supervisor_result_error(
                        "ListPanes",
                        &unexpected_supervisor_result,
                    ))
                }
            };

        let requested_pane_ids: HashSet<PaneId> = pane_ids.iter().copied().collect();
        for supervisor_pane in &supervisor_panes {
            if !requested_pane_ids.contains(&supervisor_pane.pane_id) {
                let _ = self.send_request_and_receive_result(SupervisorRequestKind::Kill {
                    pane_id: supervisor_pane.pane_id,
                    kill_policy: KillPolicy::Tree,
                });
            }
        }
        // Exits received across the pane-list reply are kept. A pane still
        // listed with an exit needs a Kill request; an unlisted pane is
        // already closed.
        let exit_status_by_pane_id = self
            .lock_link_book()
            .exit_status_by_pane_id_during_connect
            .take()
            .expect("pane reconciliation runs once");
        let mut retained_live_pane_by_id: HashMap<PaneId, LivePane> = HashMap::new();
        let mut exited_listed_pane_ids: Vec<PaneId> = Vec::new();
        for supervisor_pane in &supervisor_panes {
            if !requested_pane_ids.contains(&supervisor_pane.pane_id) {
                continue;
            }
            if exit_status_by_pane_id.contains_key(&supervisor_pane.pane_id) {
                exited_listed_pane_ids.push(supervisor_pane.pane_id);
                continue;
            }
            retained_live_pane_by_id.insert(
                supervisor_pane.pane_id,
                LivePane {
                    process_id: supervisor_pane.process_id,
                    pty_size: supervisor_pane.pty_size,
                    ungranted_output_byte_count: 0,
                },
            );
        }
        let listed_pane_ids: HashSet<PaneId> = supervisor_panes
            .iter()
            .map(|supervisor_pane| supervisor_pane.pane_id)
            .collect();
        let missing_pane_ids: Vec<PaneId> = pane_ids
            .iter()
            .filter(|pane_id| {
                !listed_pane_ids.contains(pane_id) && !exit_status_by_pane_id.contains_key(pane_id)
            })
            .copied()
            .collect();
        // The supervisor can send an exit after its ListPanes cleanup.
        for pane_id in exited_listed_pane_ids {
            self.send_request_and_require_done(SupervisorRequestKind::Kill {
                pane_id,
                kill_policy: KillPolicy::Force,
            })?;
        }
        for pane_id in missing_pane_ids {
            pty_sink.accept_exit_status(pane_id, UNOBSERVED_EXIT);
        }

        let mut link_book = self.lock_link_book();
        if link_book.link_close_cause.is_some() {
            return Err(PtyError::Io {
                detail: "the supervisor link closed while its pane list was reconciled".to_string(),
            });
        }
        link_book.live_pane_by_id = retained_live_pane_by_id;
        Ok(exit_status_by_pane_id)
    }

    /// Give the next request id to a request of `request_kind_name`, and
    /// register `answer_sender` as where its answer goes. `None` registers
    /// nothing: the request expects no answer.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the link is closed.
    fn register_request(
        &self,
        request_kind_name: &str,
        answer_sender: Option<Sender<LinkAnswer>>,
    ) -> Result<u64, PtyError> {
        let mut link_book = self.lock_link_book();
        if link_book.link_close_cause.is_some() {
            return Err(PtyError::Io {
                detail: format!(
                    "{request_kind_name} could not be sent: the supervisor link is closed"
                ),
            });
        }
        let request_id = link_book.next_request_id;
        link_book.next_request_id += 1;
        if let Some(answer_sender) = answer_sender {
            link_book
                .answer_sender_by_request_id
                .insert(request_id, answer_sender);
        }
        Ok(request_id)
    }

    /// Write one request to the link, in the link's frame protocol version.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the request cannot be encoded for a
    /// protocol 1 supervisor, or cannot be written.
    fn write_request(&self, supervisor_request: &SupervisorRequest) -> Result<(), PtyError> {
        let request_kind_name = supervisor_request.request_kind.get_request_kind_name();
        let send_result = if self.frame_protocol_version == PREVIOUS_SUPERVISOR_PROTOCOL_VERSION {
            let previous_request = serialize_previous_supervisor_request(supervisor_request)
                .map_err(|conversion_error| PtyError::Io {
                    detail: format!(
                        "{request_kind_name} cannot be encoded for the running supervisor: \
                         {conversion_error}"
                    ),
                })?;
            self.frame_writer
                .lock()
                .expect("supervisor link writer")
                .send(&previous_request)
        } else {
            self.frame_writer
                .lock()
                .expect("supervisor link writer")
                .send(supervisor_request)
        };
        send_result.map_err(|io_error| PtyError::Io {
            detail: format!("{request_kind_name} could not be sent to the supervisor: {io_error}"),
        })
    }

    /// Write one request whose answer the caller reads with
    /// [`receive_result`](Self::receive_result). The answer wait is the
    /// duration [`compute_answer_wait_duration`] gives the request, counted
    /// from the write.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the link is closed, or when the request
    /// cannot be encoded or written. A request that was not written expects
    /// no answer.
    fn send_request(&self, request_kind: SupervisorRequestKind) -> Result<SentRequest, PtyError> {
        let request_kind_name = request_kind.get_request_kind_name();
        let answer_wait_duration = compute_answer_wait_duration(&request_kind);
        let (answer_sender, answer_receiver) = channel();
        let request_id = self.register_request(request_kind_name, Some(answer_sender))?;
        if let Err(write_error) = self.write_request(&SupervisorRequest {
            request_id,
            request_kind,
        }) {
            self.lock_link_book()
                .answer_sender_by_request_id
                .remove(&request_id);
            return Err(write_error);
        }
        Ok(SentRequest {
            request_id,
            request_kind_name,
            written_at: Instant::now(),
            answer_wait_duration,
            answer_receiver,
        })
    }

    /// Write one request that the supervisor answers with nothing.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the link is closed, or when the request
    /// cannot be encoded or written.
    fn send_request_without_answer(
        &self,
        request_kind: SupervisorRequestKind,
    ) -> Result<(), PtyError> {
        let request_id = self.register_request(request_kind.get_request_kind_name(), None)?;
        self.write_request(&SupervisorRequest {
            request_id,
            request_kind,
        })
    }

    /// Wait for the answer to `sent_request`, until its answer wait runs out.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the answer does not arrive within the
    /// wait, when the link closes before the answer arrives, when an answer
    /// names a request this side never sent, when the supervisor refuses the
    /// request, or when the answer names something this build has no name
    /// for.
    fn receive_result(&self, sent_request: SentRequest) -> Result<SupervisorResult, PtyError> {
        let SentRequest {
            request_id,
            request_kind_name,
            written_at,
            answer_wait_duration,
            answer_receiver,
        } = sent_request;
        let answer_deadline = written_at + answer_wait_duration;
        let supervisor_response = match answer_receiver
            .recv_timeout(answer_deadline.saturating_duration_since(Instant::now()))
        {
            Ok(LinkAnswer::Response(supervisor_response)) => supervisor_response,
            Ok(LinkAnswer::StrayResponse {
                answered_request_id,
            }) => {
                return Err(PtyError::Io {
                    detail: format!(
                        "the supervisor answered request {answered_request_id:?} while \
                         {request_kind_name} (request {request_id}) was in flight"
                    ),
                })
            }
            Err(RecvTimeoutError::Timeout) => {
                self.lock_link_book()
                    .answer_sender_by_request_id
                    .remove(&request_id);
                return Err(PtyError::Io {
                    detail: format!(
                        "the supervisor did not answer {request_kind_name} within {} seconds",
                        answer_wait_duration.as_secs()
                    ),
                });
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(PtyError::Io {
                    detail: format!(
                        "the supervisor link closed while {request_kind_name} was in flight"
                    ),
                })
            }
        };

        match supervisor_response.answer_result {
            MaybeKnown::Known(SupervisorResult::Error(supervisor_error_payload)) => {
                Err(PtyError::Io {
                    detail: format!(
                        "the supervisor refused {request_kind_name}: {}",
                        supervisor_error_payload.message
                    ),
                })
            }
            MaybeKnown::Known(supervisor_result) => Ok(supervisor_result),
            MaybeKnown::Unknown {
                variant_name: answer_variant_name,
            } => Err(PtyError::Io {
                detail: format!(
                    "the supervisor answered {request_kind_name} with {answer_variant_name}, \
                     which this build has no name for"
                ),
            }),
        }
    }

    /// Send one request and wait for its answer
    /// ([`send_request`](Self::send_request), then
    /// [`receive_result`](Self::receive_result)).
    ///
    /// # Errors
    /// Returns whatever those two report.
    fn send_request_and_receive_result(
        &self,
        request_kind: SupervisorRequestKind,
    ) -> Result<SupervisorResult, PtyError> {
        let sent_request = self.send_request(request_kind)?;
        self.receive_result(sent_request)
    }

    /// Send one request whose only good response is
    /// [`SupervisorResult::Done`], and hand back nothing else.
    ///
    /// # Errors
    /// Returns whatever [`send_request_and_receive_result`](Self::send_request_and_receive_result)
    /// reports, and [`PtyError::Io`] naming both the request and a response that is not `Done`.
    fn send_request_and_require_done(
        &self,
        request_kind: SupervisorRequestKind,
    ) -> Result<(), PtyError> {
        let request_kind_name = request_kind.get_request_kind_name();
        let supervisor_result = self.send_request_and_receive_result(request_kind)?;
        require_done_result(request_kind_name, supervisor_result)
    }
}

impl PtyBackend for SupervisorPtyBackend {
    /// Ask the supervisor to open a pane: it makes a terminal of `pty_size` and
    /// launches `spawn_spec` inside it.
    ///
    /// The child runs in the supervisor's process, not this one, and its output
    /// and exit arrive on the link and go to the backend's sink.
    ///
    /// # Errors
    /// Returns [`PtyError::Spawn`] when the supervisor refuses, when it does
    /// not answer within the answer wait, when the link fails, or when the
    /// link closed after the supervisor answered; the detail names which.
    /// Returns [`PtyError::Io`] when the supervisor answers with something
    /// other than [`SupervisorResult::Spawned`].
    ///
    /// # Panics
    /// In debug builds, panics when `pane_id` is already live in this backend.
    fn spawn_pane(
        &self,
        pane_id: PaneId,
        spawn_spec: SpawnSpec,
        pty_size: PtySize,
    ) -> Result<(), PtyError> {
        debug_assert!(
            !self
                .link
                .lock_link_book()
                .live_pane_by_id
                .contains_key(&pane_id),
            "spawn into an already-live pane id {pane_id}; kill it before respawning"
        );
        let supervisor_result =
            self.link
                .send_request_and_receive_result(SupervisorRequestKind::Spawn {
                    pane_id,
                    spawn_spec,
                    pty_size,
                });
        let process_id = match supervisor_result {
            Ok(SupervisorResult::Spawned { process_id }) => process_id,
            Ok(unexpected_supervisor_result) => {
                return Err(build_unexpected_supervisor_result_error(
                    "Spawn",
                    &unexpected_supervisor_result,
                ))
            }
            // Every failure of the exchange is reported as a spawn failure.
            Err(PtyError::Io { detail }) => return Err(PtyError::Spawn { detail }),
            Err(pty_error) => return Err(pty_error),
        };
        let mut link_book = self.link.lock_link_book();
        if link_book.link_close_cause.is_some() {
            return Err(PtyError::Spawn {
                detail: format!(
                    "the supervisor link closed after pane {pane_id} was opened, so nothing \
                     carries its output"
                ),
            });
        }
        link_book.live_pane_by_id.insert(
            pane_id,
            LivePane {
                process_id,
                pty_size,
                ungranted_output_byte_count: 0,
            },
        );
        Ok(())
    }

    /// Retune a pane's terminal, which its child sees as a window-size change.
    ///
    /// The new size is recorded after the supervisor answers `Done`. A refused
    /// resize leaves the recorded size unchanged.
    ///
    /// # Errors
    /// Returns [`PtyError::UnknownPane`] when this backend does not drive
    /// `pane_id`, and [`PtyError::Io`] when the supervisor refuses or the link
    /// fails.
    fn resize_pane(&self, pane_id: PaneId, pty_size: PtySize) -> Result<(), PtyError> {
        self.validate_pane_is_driven(pane_id)?;
        self.link
            .send_request_and_require_done(SupervisorRequestKind::Resize { pane_id, pty_size })?;
        self.record_pane_size(pane_id, pty_size);
        Ok(())
    }

    /// Retune several panes' terminals with one round trip: every Resize
    /// request is written before the first answer is read.
    ///
    /// Each entry's result is the one [`resize_pane`](PtyBackend::resize_pane)
    /// gives it. A pane this backend does not drive is refused without a
    /// request.
    fn resize_panes(&self, pane_pty_sizes: &[(PaneId, PtySize)]) -> Vec<Result<(), PtyError>> {
        let sent_requests: Vec<Result<SentRequest, PtyError>> = pane_pty_sizes
            .iter()
            .map(|&(pane_id, pty_size)| {
                self.validate_pane_is_driven(pane_id)?;
                self.link
                    .send_request(SupervisorRequestKind::Resize { pane_id, pty_size })
            })
            .collect();
        sent_requests
            .into_iter()
            .zip(pane_pty_sizes)
            .map(|(sent_request, &(pane_id, pty_size))| {
                let supervisor_result = self.link.receive_result(sent_request?)?;
                require_done_result("Resize", supervisor_result)?;
                self.record_pane_size(pane_id, pty_size);
                Ok(())
            })
            .collect()
    }

    /// Send `input_bytes` to a pane's child, which reach it as typed input.
    ///
    /// # Errors
    /// Returns [`PtyError::UnknownPane`] when this backend does not drive
    /// `pane_id`, and [`PtyError::Io`] when the supervisor refuses or the link
    /// fails.
    fn write_pane_input(&self, pane_id: PaneId, input_bytes: &[u8]) -> Result<(), PtyError> {
        self.validate_pane_is_driven(pane_id)?;
        self.link
            .send_request_and_require_done(SupervisorRequestKind::Write {
                pane_id,
                input_bytes: input_bytes.to_vec(),
            })
    }

    /// End a pane's child according to `kill_policy` and drop the pane.
    ///
    /// The pane leaves this backend whatever the supervisor answers. No output
    /// and no exit for that pane reaches the sink afterwards.
    ///
    /// # Errors
    /// Returns [`PtyError::UnknownPane`] when this backend does not drive
    /// `pane_id`, and [`PtyError::Io`] when the supervisor refuses or the link
    /// fails.
    fn kill_pane(&self, pane_id: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError> {
        if self
            .link
            .lock_link_book()
            .live_pane_by_id
            .remove(&pane_id)
            .is_none()
        {
            return Err(PtyError::UnknownPane { pane_id });
        }
        self.link
            .send_request_and_require_done(SupervisorRequestKind::Kill {
                pane_id,
                kill_policy,
            })
    }

    /// The live working directory of `pane_id`'s child, asked from the operating
    /// system by the supervisor, which is the child's parent. `None` when this
    /// backend does not drive `pane_id`, the pane has no live child, the platform
    /// has no lookup, the supervisor refuses, or the link fails.
    fn find_live_working_directory(&self, pane_id: PaneId) -> Option<PathBuf> {
        self.validate_pane_is_driven(pane_id).ok()?;
        match self
            .link
            .send_request_and_receive_result(SupervisorRequestKind::LiveCwd { pane_id })
        {
            Ok(SupervisorResult::Cwd(working_directory_path)) => working_directory_path,
            _ => None,
        }
    }

    /// Add `parsed_byte_count` to what `pane_id` has parsed and not granted
    /// back. Once that reaches [`MINIMUM_OUTPUT_GRANT_BYTE_COUNT`], all of it
    /// goes to the supervisor in one
    /// [`GrantPaneOutput`](SupervisorRequestKind::GrantPaneOutput), which
    /// expects no answer, and the count starts again at 0.
    ///
    /// Does nothing on a link below [`OUTPUT_WINDOW_PROTOCOL_VERSION`], or
    /// for a pane this backend does not drive. A grant that cannot be written
    /// is dropped: the link is broken, and the link reader reports it.
    ///
    /// Example — grants of 40 KiB and then 30 KiB send one GrantPaneOutput of
    /// 70 KiB, after the second.
    fn grant_pane_output(&self, pane_id: PaneId, parsed_byte_count: usize) {
        if !self.has_output_windows {
            return;
        }
        let granted_byte_count = {
            let mut link_book = self.link.lock_link_book();
            let Some(live_pane) = link_book.live_pane_by_id.get_mut(&pane_id) else {
                return;
            };
            live_pane.ungranted_output_byte_count += parsed_byte_count;
            if live_pane.ungranted_output_byte_count < MINIMUM_OUTPUT_GRANT_BYTE_COUNT as usize {
                return;
            }
            std::mem::take(&mut live_pane.ungranted_output_byte_count)
        };
        let _ = self
            .link
            .send_request_without_answer(SupervisorRequestKind::GrantPaneOutput {
                pane_id,
                granted_byte_count: u32::try_from(granted_byte_count).unwrap_or(u32::MAX),
            });
    }
}

/// `Ok(())` for [`SupervisorResult::Done`].
///
/// # Errors
/// Returns [`PtyError::Io`] naming `request_kind_name` and any other result.
fn require_done_result(
    request_kind_name: &str,
    supervisor_result: SupervisorResult,
) -> Result<(), PtyError> {
    match supervisor_result {
        SupervisorResult::Done => Ok(()),
        unexpected_supervisor_result => Err(build_unexpected_supervisor_result_error(
            request_kind_name,
            &unexpected_supervisor_result,
        )),
    }
}

/// How long `request_kind` waits for its response: [`ANSWER_WAIT_DURATION`], plus the grace window
/// of a kill that asks the child to exit on its own.
///
/// The supervisor answers such a kill after it has spent that window.
fn compute_answer_wait_duration(request_kind: &SupervisorRequestKind) -> Duration {
    match request_kind {
        SupervisorRequestKind::Kill {
            kill_policy:
                KillPolicy::Graceful {
                    timeout_duration: kill_timeout_duration,
                }
                | KillPolicy::GracefulTree {
                    timeout_duration: kill_timeout_duration,
                },
            ..
        } => ANSWER_WAIT_DURATION + *kill_timeout_duration,
        _ => ANSWER_WAIT_DURATION,
    }
}

/// The failure for an answer that does not fit the request it answers, naming
/// both. The answer is named by its variant alone; no payload reaches the
/// message.
fn build_unexpected_supervisor_result_error(
    request_kind_name: &str,
    supervisor_result: &SupervisorResult,
) -> PtyError {
    PtyError::Io {
        detail: format!(
            "the supervisor answered {request_kind_name} with {}",
            supervisor_result.get_wire_name()
        ),
    }
}

/// Start the thread that reads the link: it hands each response to the caller
/// whose request it names and each pane event to `pty_sink`.
///
/// Every frame is parsed in `frame_protocol_version`. On a link of the current
/// version, a binary frame is one chunk of one pane's output
/// ([`decode_pane_output_frame`]).
///
/// A response is handled by the id it names:
///
/// - An id with a caller waiting goes to that caller.
/// - An id this side gave out with no caller waiting is passed over: that
///   caller's wait ran out, or the request expects no answer.
/// - An id this side never gave out, and a response that names no id, go to
///   every caller waiting at that moment as
///   [`LinkAnswer::StrayResponse`].
///
/// An event this build has no name for is passed over, and the link keeps
/// carrying the rest. A pane whose output chunk `pty_sink` refused takes
/// nothing more, its exit included; every other pane keeps being delivered.
/// A pane takes at most one exit.
///
/// The thread ends when the link breaks or a frame does not parse. Then it
/// records the cause in `link_book`, which fails every caller waiting, and
/// reports every pane still in the book as the backend's
/// [type documentation](SupervisorPtyBackend) states.
///
/// # Panics
/// Panics when the operating system cannot start the thread.
fn start_link_reader_thread(
    mut frame_reader: FrameReader,
    link_book: Arc<Mutex<LinkBook>>,
    pty_sink: Arc<dyn PtySink>,
    frame_protocol_version: u32,
) {
    let _ = thread::Builder::new()
        .name("koshi-pty-link".to_string())
        .spawn(move || {
            let mut output_rejected_pane_ids: HashSet<PaneId> = HashSet::new();
            let mut exited_pane_ids: HashSet<PaneId> = HashSet::new();
            let link_close_cause = loop {
                let received_frame =
                    if frame_protocol_version == PREVIOUS_SUPERVISOR_PROTOCOL_VERSION {
                        frame_reader
                            .recv::<PreviousSupervisorMessage>()
                            .map(|previous_message| {
                                ReceivedFrame::Message(previous_message.supervisor_message)
                            })
                    } else {
                        frame_reader.recv_message_or_binary::<IncomingSupervisorMessage>()
                    };
                let supervisor_message = match received_frame {
                    Ok(ReceivedFrame::Message(supervisor_message)) => supervisor_message,
                    Ok(ReceivedFrame::Binary(frame_bytes)) => {
                        match decode_pane_output_frame(frame_bytes) {
                            Ok((pane_id, output_bytes)) => SupervisorMessage::Event(
                                MaybeKnown::Known(SupervisorEvent::Output {
                                    pane_id,
                                    output_bytes,
                                }),
                            ),
                            Err(_) => break LinkCloseCause::UnreadableFrame,
                        }
                    }
                    Err(IpcError::MalformedFrame { .. }) => break LinkCloseCause::UnreadableFrame,
                    Err(_) => break LinkCloseCause::ReadFailed,
                };
                match supervisor_message {
                    SupervisorMessage::Response(supervisor_response) => {
                        deliver_supervisor_response(&link_book, supervisor_response);
                    }
                    SupervisorMessage::Event(MaybeKnown::Known(SupervisorEvent::Output {
                        pane_id,
                        output_bytes,
                    })) => {
                        if !output_rejected_pane_ids.contains(&pane_id)
                            && !pty_sink.accept_output_bytes(pane_id, output_bytes)
                        {
                            output_rejected_pane_ids.insert(pane_id);
                        }
                    }
                    SupervisorMessage::Event(MaybeKnown::Known(SupervisorEvent::Exited {
                        pane_id,
                        exit_status,
                    })) => {
                        if let Some(exit_status_by_pane_id) = link_book
                            .lock()
                            .expect("supervisor link book")
                            .exit_status_by_pane_id_during_connect
                            .as_mut()
                        {
                            exit_status_by_pane_id.entry(pane_id).or_insert(exit_status);
                        }
                        if !output_rejected_pane_ids.contains(&pane_id)
                            && exited_pane_ids.insert(pane_id)
                        {
                            pty_sink.accept_exit_status(pane_id, exit_status);
                        }
                    }
                    SupervisorMessage::Event(MaybeKnown::Unknown { .. }) => {}
                }
            };

            let lost_pane_ids: Vec<PaneId> = {
                let mut link_book = link_book.lock().expect("supervisor link book");
                link_book.link_close_cause = Some(link_close_cause);
                link_book.answer_sender_by_request_id.clear();
                link_book
                    .live_pane_by_id
                    .drain()
                    .map(|(pane_id, _)| pane_id)
                    .filter(|pane_id| {
                        !output_rejected_pane_ids.contains(pane_id)
                            && !exited_pane_ids.contains(pane_id)
                    })
                    .collect()
            };
            if !lost_pane_ids.is_empty() {
                tracing::warn!(
                    ?link_close_cause,
                    lost_pane_count = lost_pane_ids.len(),
                    "the link to the process holding the panes closed; every pane it still \
                     carried is reported as ended"
                );
            }
            for pane_id in lost_pane_ids {
                pty_sink.accept_exit_status(pane_id, UNOBSERVED_EXIT);
            }
        })
        .expect("spawn supervisor link reader thread");
}

/// Hand `supervisor_response` to the caller whose request it names, as
/// [`start_link_reader_thread`] lists by id.
fn deliver_supervisor_response(
    link_book: &Mutex<LinkBook>,
    supervisor_response: SupervisorResponse<MaybeKnown<SupervisorResult>>,
) {
    let mut link_book = link_book.lock().expect("supervisor link book");
    if let Some(answered_request_id) = supervisor_response.request_id {
        if let Some(answer_sender) = link_book
            .answer_sender_by_request_id
            .remove(&answered_request_id)
        {
            let _ = answer_sender.send(LinkAnswer::Response(supervisor_response));
            return;
        }
        if answered_request_id < link_book.next_request_id {
            return;
        }
    }
    for (_, answer_sender) in link_book.answer_sender_by_request_id.drain() {
        let _ = answer_sender.send(LinkAnswer::StrayResponse {
            answered_request_id: supervisor_response.request_id,
        });
    }
}

#[cfg(test)]
mod tests;
