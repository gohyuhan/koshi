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
//! and its answer, and every byte a pane prints arrives as an event handed to
//! the [`PtySink`](crate::backend::state::PtySink) the backend was built with.
//! Under the same sink it behaves as
//! [`PortablePtyBackend`](crate::portable::PortablePtyBackend) does.
//!
//! The process at the other end speaks [`koshi_ipc::supervisor`]'s protocol;
//! koshi's own binary runs it under a hidden subcommand.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use koshi_core::ids::PaneId;
use koshi_core::process::{KillPolicy, PtySize, SpawnSpec};
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::supervisor::{
    IncomingSupervisorMessage, SupervisorEvent, SupervisorMessage, SupervisorRequest,
    SupervisorRequestKind, SupervisorResponse, SupervisorResult,
};
use koshi_ipc::transport::{Connection, FrameReader, FrameWriter};
use koshi_ipc::wire::{MaybeKnown, WireName};

use crate::backend::state::{CarriedPtyPane, PtyBackend, PtyHandle, PtySink, UNOBSERVED_EXIT};
use crate::error::PtyError;

/// How long one request waits for its answer. A request not answered within
/// this window is reported as failed.
///
/// The supervisor serves one link at a time. The window includes the time it
/// is still spending on the link a replaced process image left behind.
const ANSWER_WAIT_DURATION: Duration = Duration::from_secs(10);

/// What this side keeps for one pane the supervisor holds.
///
/// The supervisor is the authority on the pane itself.
/// [`carried_panes`](SupervisorPtyBackend::list_carried_panes) reads these two
/// facts without a round trip.
#[derive(Debug, Clone, Copy)]
struct LivePane {
    /// The process id of the pane's child, as the supervisor reported it.
    process_id: u32,
    /// The last size this pane's terminal was set to: what it was spawned or
    /// taken back at, then whatever the newest successful
    /// [`resize_pane`](PtyBackend::resize_pane) carried.
    pty_size: PtySize,
}

/// The link, held under one lock: one request is in flight at a time.
///
/// The lock is held from writing a request to reading its answer. The answer a
/// caller reads is the one to its own request.
struct Link {
    /// The writing half: every request goes out here.
    frame_writer: FrameWriter,
    /// The answers the reader thread hands over, in arrival order.
    response_receiver: Receiver<SupervisorResponse<MaybeKnown<SupervisorResult>>>,
    /// The id the next request carries.
    next_request_id: u64,
}

/// A [`PtyBackend`] whose panes live in a supervisor process.
///
/// Every pane's pseudo-terminal is opened and closed by that process and by no
/// other. This backend's own process can exit and be replaced while every pane
/// keeps running. A replacement image calls [`connect`](Self::connect) again
/// and drives the same panes.
///
/// One process builds one of these and keeps it. Dropping it leaves the link's
/// reader thread holding the reading half. The link closes when this process
/// exits or when [`shutdown_supervisor`](Self::shutdown_supervisor) ends the supervisor.
pub struct SupervisorPtyBackend {
    /// The link to the supervisor.
    link: Mutex<Link>,
    /// The panes this backend believes the supervisor holds, keyed by pane id. A
    /// [`spawn_pane`](PtyBackend::spawn_pane) adds one and a [`kill_pane`](PtyBackend::kill_pane)
    /// removes one.
    live_panes_by_id: Mutex<HashMap<PaneId, LivePane>>,
    /// Where every pane's output and exit is delivered.
    /// [`connect`](Self::connect) reports a pane the supervisor no longer has
    /// as ended through it.
    pty_sink: Arc<dyn PtySink>,
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
    ///   as ended, carrying `ExitCode(-1)` — the status a child that cannot be
    ///   waited on reports. Every kill above is sent first.
    ///
    /// Every remaining pane is driven by the returned backend, at the process
    /// id and size the supervisor listed.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the link cannot be opened, when the
    /// supervisor does not answer within the answer wait, when it refuses the
    /// Hello or the pane list, or when it answers either with something else.
    /// Any of those closes the link's read direction, so the reader thread
    /// ends and the supervisor is free to serve the next link.
    ///
    /// # Panics
    /// Panics when the operating system cannot start the link's reader thread.
    pub fn connect(
        supervisor_address: &str,
        connection_token: ConnectionToken,
        pty_sink: Arc<dyn PtySink>,
        pane_ids: &[PaneId],
    ) -> Result<SupervisorPtyBackend, PtyError> {
        let connection =
            Connection::connect(supervisor_address).map_err(|io_error| PtyError::Io {
                detail: format!(
                    "the supervisor at {supervisor_address} could not be reached: {io_error}"
                ),
            })?;
        let link_closer = connection.read_closer().ok();
        let (frame_reader, frame_writer) = connection.split();
        let (response_sender, response_receiver) = channel();
        start_link_reader_thread(frame_reader, response_sender, Arc::clone(&pty_sink));

        let backend = SupervisorPtyBackend {
            link: Mutex::new(Link {
                frame_writer,
                response_receiver,
                next_request_id: 1,
            }),
            live_panes_by_id: Mutex::new(HashMap::new()),
            pty_sink,
        };

        match backend.reconcile_panes(connection_token, pane_ids) {
            Ok(()) => Ok(backend),
            Err(reconcile_error) => {
                if let Some(link_closer) = link_closer {
                    link_closer.close();
                }
                Err(reconcile_error)
            }
        }
    }

    /// Present `connection_token`, read the pane list, and reconcile it against
    /// `pane_ids`, the panes the caller believes are running.
    ///
    /// A pane the supervisor holds that `pane_ids` does not name is killed with
    /// [`KillPolicy::Tree`]; a pane `pane_ids` names that the supervisor does not
    /// hold is reported to the `pty_sink` as ended. Every remaining pane is written
    /// into this backend's pane map.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the supervisor refuses the Hello or the
    /// pane list, answers either with something else, or does not answer.
    fn reconcile_panes(
        &self,
        connection_token: ConnectionToken,
        pane_ids: &[PaneId],
    ) -> Result<(), PtyError> {
        match self.send_request_and_receive_result(SupervisorRequestKind::build_hello_request(
            connection_token,
        ))? {
            SupervisorResult::Hello { .. } => {}
            unexpected_supervisor_result => {
                return Err(build_unexpected_supervisor_result_error(
                    "Hello",
                    &unexpected_supervisor_result,
                ))
            }
        }
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

        // Both differences are settled before any pane is driven.
        let requested_pane_ids: HashSet<PaneId> = pane_ids.iter().copied().collect();
        for supervisor_pane in &supervisor_panes {
            if !requested_pane_ids.contains(&supervisor_pane.pane_id) {
                let _ = self.send_request_and_receive_result(SupervisorRequestKind::Kill {
                    pane_id: supervisor_pane.pane_id,
                    kill_policy: KillPolicy::Tree,
                });
            }
        }
        let retained_live_panes_by_id: HashMap<PaneId, LivePane> = supervisor_panes
            .iter()
            .filter(|supervisor_pane| requested_pane_ids.contains(&supervisor_pane.pane_id))
            .map(|supervisor_pane| {
                (
                    supervisor_pane.pane_id,
                    LivePane {
                        process_id: supervisor_pane.process_id,
                        pty_size: supervisor_pane.pty_size,
                    },
                )
            })
            .collect();
        for pane_id in pane_ids
            .iter()
            .filter(|pane_id| !retained_live_panes_by_id.contains_key(pane_id))
        {
            self.pty_sink.accept_exit_status(*pane_id, UNOBSERVED_EXIT);
        }
        *self.live_panes_by_id.lock().expect("supervisor panes") = retained_live_panes_by_id;

        Ok(())
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
        self.send_request_and_require_done(SupervisorRequestKind::PauseOutput)
    }

    /// Put every held reader back to work: the supervisor writes what it held
    /// to this link and keeps writing.
    ///
    /// A refusal, a link that broke, and an answer that never came are all
    /// dropped here. Each one is a supervisor no longer serving this session,
    /// which every subsequent request on this link reports in turn.
    pub fn resume_readers(&self) {
        let _ = self.send_request_and_receive_result(SupervisorRequestKind::ResumeOutput);
    }

    /// Wait until no byte this backend took for a child is still queued.
    ///
    /// [`write_pane_input`](PtyBackend::write_pane_input) sends the bytes to the supervisor and waits
    /// for its answer. A write that has returned is already the supervisor's,
    /// and this process queues nothing. The supervisor keeps running across an
    /// image swap, and its own writer threads carry those bytes to the
    /// terminals.
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
        let live_panes_by_id = self.live_panes_by_id.lock().expect("supervisor panes");
        live_panes_by_id
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
        match self.send_request_and_receive_result(SupervisorRequestKind::Shutdown) {
            Ok(SupervisorResult::Done) => Ok(()),
            Ok(unexpected_result) => Err(build_unexpected_supervisor_result_error(
                "Shutdown",
                &unexpected_result,
            )),
            Err(_) => Ok(()),
        }
    }

    /// Send one request and wait for its response, for at most the duration
    /// [`compute_answer_wait_duration`] gives that request. The window starts once
    /// the link lock is taken, so time spent waiting behind another caller's
    /// exchange is not charged against it.
    ///
    /// The link lock is held for the whole exchange: two callers never read
    /// each other's answers. An answer carrying the id of an earlier request of
    /// this side is passed over; it answers a request whose wait ran out. The
    /// link stays in step after such a wait.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the request cannot be written, when the
    /// answer does not arrive within the wait, when the link closes before the
    /// answer arrives, when the answer names a request this side never sent,
    /// when the supervisor refuses the request, or when the answer names
    /// something this build has no name for.
    fn send_request_and_receive_result(
        &self,
        request_kind: SupervisorRequestKind,
    ) -> Result<SupervisorResult, PtyError> {
        let request_kind_name = request_kind.get_request_kind_name();
        let wait_duration = compute_answer_wait_duration(&request_kind);
        let mut link_state = self.link.lock().expect("supervisor link");
        let deadline = Instant::now() + wait_duration;
        let request_id = link_state.next_request_id;
        link_state.next_request_id += 1;
        link_state
            .frame_writer
            .send(&SupervisorRequest {
                request_id,
                request_kind,
            })
            .map_err(|io_error| PtyError::Io {
                detail: format!(
                    "{request_kind_name} could not be sent to the supervisor: {io_error}"
                ),
            })?;
        let supervisor_response = loop {
            let remaining_wait_duration = deadline.saturating_duration_since(Instant::now());
            match link_state
                .response_receiver
                .recv_timeout(remaining_wait_duration)
            {
                Ok(supervisor_response) if supervisor_response.request_id == Some(request_id) => {
                    break supervisor_response
                }
                // The response to an earlier request of this side's own, whose
                // wait already ran out.
                Ok(supervisor_response)
                    if supervisor_response
                        .request_id
                        .is_some_and(|supervisor_response_id| {
                            supervisor_response_id < request_id
                        }) => {}
                Ok(supervisor_response) => {
                    return Err(PtyError::Io {
                        detail: format!(
                            "the supervisor answered request {:?} while {request_kind_name} \
                             (request {request_id}) was in flight",
                            supervisor_response.request_id
                        ),
                    })
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(PtyError::Io {
                        detail: format!(
                            "the supervisor did not answer {request_kind_name} within {} seconds",
                            wait_duration.as_secs()
                        ),
                    })
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(PtyError::Io {
                        detail: format!(
                            "the supervisor link closed while {request_kind_name} was in flight"
                        ),
                    })
                }
            }
        };
        drop(link_state);

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
        match self.send_request_and_receive_result(request_kind)? {
            SupervisorResult::Done => Ok(()),
            unexpected_supervisor_result => Err(build_unexpected_supervisor_result_error(
                request_kind_name,
                &unexpected_supervisor_result,
            )),
        }
    }

    /// [`PtyError::UnknownPane`] when this backend does not drive `pane_id`.
    /// Checked before every request that names one pane.
    fn validate_pane_is_driven(&self, pane_id: PaneId) -> Result<(), PtyError> {
        if self
            .live_panes_by_id
            .lock()
            .expect("supervisor panes")
            .contains_key(&pane_id)
        {
            Ok(())
        } else {
            Err(PtyError::UnknownPane { pane_id })
        }
    }
}

impl PtyBackend for SupervisorPtyBackend {
    /// Ask the supervisor to open a pane: it makes a terminal of `pty_size` and
    /// launches `spawn_spec` inside it.
    ///
    /// The child runs in the supervisor's process, not this one, and its output
    /// and exit arrive as events on the link. The returned handle is
    /// [`PtyHandle::from_detached_pane_id`]: it carries no channels, and the caller starts
    /// no relay thread for the pane.
    ///
    /// # Errors
    /// Returns [`PtyError::Spawn`] when the supervisor refuses, when it does
    /// not answer within the answer wait, or when the link fails; the detail
    /// names which. Returns [`PtyError::Io`] when the supervisor answers with
    /// something other than [`SupervisorResult::Spawned`].
    ///
    /// # Panics
    /// In debug builds, panics when `pane_id` is already live in this backend.
    fn spawn_pane(
        &self,
        pane_id: PaneId,
        spawn_spec: SpawnSpec,
        pty_size: PtySize,
    ) -> Result<PtyHandle, PtyError> {
        debug_assert!(
            !self
                .live_panes_by_id
                .lock()
                .expect("supervisor panes")
                .contains_key(&pane_id),
            "spawn into an already-live pane id {pane_id}; kill it before respawning"
        );
        let supervisor_result =
            self.send_request_and_receive_result(SupervisorRequestKind::Spawn {
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
        self.live_panes_by_id
            .lock()
            .expect("supervisor panes")
            .insert(
                pane_id,
                LivePane {
                    process_id,
                    pty_size,
                },
            );
        Ok(PtyHandle::from_detached_pane_id(pane_id))
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
        self.send_request_and_require_done(SupervisorRequestKind::Resize { pane_id, pty_size })?;
        if let Some(live_pane) = self
            .live_panes_by_id
            .lock()
            .expect("supervisor panes")
            .get_mut(&pane_id)
        {
            live_pane.pty_size = pty_size;
        }
        Ok(())
    }

    /// Send `input_bytes` to a pane's child, which reach it as typed input.
    ///
    /// # Errors
    /// Returns [`PtyError::UnknownPane`] when this backend does not drive
    /// `pane_id`, and [`PtyError::Io`] when the supervisor refuses or the link
    /// fails.
    fn write_pane_input(&self, pane_id: PaneId, input_bytes: &[u8]) -> Result<(), PtyError> {
        self.validate_pane_is_driven(pane_id)?;
        self.send_request_and_require_done(SupervisorRequestKind::Write {
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
            .live_panes_by_id
            .lock()
            .expect("supervisor panes")
            .remove(&pane_id)
            .is_none()
        {
            return Err(PtyError::UnknownPane { pane_id });
        }
        self.send_request_and_require_done(SupervisorRequestKind::Kill {
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
        match self.send_request_and_receive_result(SupervisorRequestKind::LiveCwd { pane_id }) {
            Ok(SupervisorResult::Cwd(working_directory_path)) => working_directory_path,
            _ => None,
        }
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
            supervisor_result.wire_name()
        ),
    }
}

/// Start the thread that reads the link: it hands each response to whoever is
/// waiting on [`Link::response_receiver`] and each event to `pty_sink`.
///
/// The thread ends when the link breaks, when a frame does not decode, or when
/// no one holds the receiving end of `answers`. Ending drops `answers`: a
/// caller waiting for an answer reads the link as closed. An event this build
/// has no name for is passed over, and the link keeps carrying the rest.
///
/// A pane whose output chunk `pty_sink` refused takes nothing more, its exit included;
/// every other pane keeps being delivered.
///
/// # Panics
/// Panics when the operating system cannot start the thread.
fn start_link_reader_thread(
    mut frame_reader: FrameReader,
    response_sender: Sender<SupervisorResponse<MaybeKnown<SupervisorResult>>>,
    pty_sink: Arc<dyn PtySink>,
) {
    let _ = thread::Builder::new()
        .name("koshi-pty-link".to_string())
        .spawn(move || {
            // A pane whose chunk the consumer refused. Nothing more of that
            // pane is delivered, its exit included; every other pane keeps
            // being delivered.
            let mut output_rejected_pane_ids: HashSet<PaneId> = HashSet::new();
            while let Ok(incoming_supervisor_message) =
                frame_reader.recv::<IncomingSupervisorMessage>()
            {
                match incoming_supervisor_message {
                    SupervisorMessage::Response(supervisor_response) => {
                        if response_sender.send(supervisor_response).is_err() {
                            return;
                        }
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
                        if !output_rejected_pane_ids.contains(&pane_id) {
                            pty_sink.accept_exit_status(pane_id, exit_status);
                        }
                    }
                    SupervisorMessage::Event(MaybeKnown::Unknown { .. }) => {}
                }
            }
        })
        .expect("spawn supervisor link reader thread");
}

#[cfg(test)]
mod tests;
