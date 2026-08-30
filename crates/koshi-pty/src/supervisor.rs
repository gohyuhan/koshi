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
use koshi_core::process::{ExitStatus, KillPolicy, PtySize, SpawnSpec};
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::supervisor::{
    IncomingSupervisorMessage, SupervisorEvent, SupervisorMessage, SupervisorRequest,
    SupervisorRequestKind, SupervisorResponse, SupervisorResult,
};
use koshi_ipc::transport::{Connection, FrameReader, FrameWriter};
use koshi_ipc::wire::{MaybeKnown, WireName};

use crate::backend::state::{PtyBackend, PtyHandle, PtySink};
use crate::error::PtyError;
use crate::portable::CarriedPtyPane;

/// The exit reported for a pane the supervisor no longer holds: no session
/// server observed the child's status.
///
/// The same value a Unix pane taken back through `PortablePtyBackend::adopt`
/// reports for a child that cannot be waited on.
const UNOBSERVED_EXIT: ExitStatus = ExitStatus::ExitCode(-1);

/// How long one request waits for its answer. A request not answered within
/// this window is reported as failed.
///
/// The supervisor serves one link at a time. The window includes the time it
/// is still spending on the link a replaced process image left behind.
const ANSWER_WAIT: Duration = Duration::from_secs(10);

/// What this side keeps for one pane the supervisor holds.
///
/// The supervisor is the authority on the pane itself.
/// [`carried_panes`](SupervisorPtyBackend::carried_panes) reads these two
/// facts without a round trip.
#[derive(Debug, Clone, Copy)]
struct LivePane {
    /// The process id of the pane's child, as the supervisor reported it.
    pid: u32,
    /// The last size this pane's terminal was set to: what it was spawned or
    /// taken back at, then whatever the newest successful
    /// [`resize`](PtyBackend::resize) carried.
    size: PtySize,
}

/// The link, held under one lock: one request is in flight at a time.
///
/// The lock is held from writing a request to reading its answer. The answer a
/// caller reads is the one to its own request.
struct Link {
    /// The writing half: every request goes out here.
    writer: FrameWriter,
    /// The answers the reader thread hands over, in arrival order.
    answers: Receiver<SupervisorResponse<MaybeKnown<SupervisorResult>>>,
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
/// exits or when [`shut_down`](Self::shut_down) ends the supervisor.
pub struct SupervisorPtyBackend {
    /// The link to the supervisor.
    link: Mutex<Link>,
    /// The panes this backend believes the supervisor holds, keyed by id. A
    /// [`spawn`](PtyBackend::spawn) adds one and a [`kill`](PtyBackend::kill)
    /// removes one.
    panes: Mutex<HashMap<PaneId, LivePane>>,
    /// Where every pane's output and exit is delivered.
    /// [`connect`](Self::connect) reports a pane the supervisor no longer has
    /// as ended through it.
    sink: Arc<dyn PtySink>,
}

impl SupervisorPtyBackend {
    /// Open a link to the supervisor listening at `addr`, present `token`, and
    /// settle which panes this backend drives.
    ///
    /// `panes` is what the caller believes is running: empty for a session
    /// starting fresh, and the carried pane list for one that has just replaced
    /// its own image. It is settled against what the supervisor holds:
    ///
    /// - A pane the supervisor holds that is not in `panes` is killed with
    ///   [`KillPolicy::Tree`], in the order the supervisor listed it. The
    ///   answer to that kill is not checked.
    /// - A pane in `panes` the supervisor does not hold is reported to `sink`
    ///   as ended, carrying [`ExitStatus::ExitCode`]`(-1)`: the status a child
    ///   that cannot be waited on reports. Every kill above is sent first.
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
        addr: &str,
        token: ConnectionToken,
        sink: Arc<dyn PtySink>,
        panes: &[PaneId],
    ) -> Result<SupervisorPtyBackend, PtyError> {
        let connection = Connection::connect(addr).map_err(|error| PtyError::Io {
            detail: format!("the supervisor at {addr} could not be reached: {error}"),
        })?;
        let closer = connection.read_closer().ok();
        let (reader, writer) = connection.split();
        let (answers_tx, answers) = channel();
        start_link_reader(reader, answers_tx, Arc::clone(&sink));

        let backend = SupervisorPtyBackend {
            link: Mutex::new(Link {
                writer,
                answers,
                next_request_id: 1,
            }),
            panes: Mutex::new(HashMap::new()),
            sink,
        };

        match backend.settle(token, panes) {
            Ok(()) => Ok(backend),
            Err(error) => {
                if let Some(closer) = closer {
                    closer.close();
                }
                Err(error)
            }
        }
    }

    /// Present `token`, read the pane list, and settle it against `panes`, the
    /// panes the caller believes are running.
    ///
    /// A pane the supervisor holds that `panes` does not name is killed with
    /// [`KillPolicy::Tree`]; a pane `panes` names that the supervisor does not
    /// hold is reported to the sink as ended. Every remaining pane is written
    /// into this backend's pane map.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the supervisor refuses the Hello or the
    /// pane list, answers either with something else, or does not answer.
    fn settle(&self, token: ConnectionToken, panes: &[PaneId]) -> Result<(), PtyError> {
        match self.ask(SupervisorRequestKind::hello(token))? {
            SupervisorResult::Hello { .. } => {}
            other => return Err(unexpected_answer("Hello", &other)),
        }
        let held = match self.ask(SupervisorRequestKind::ListPanes)? {
            SupervisorResult::Panes(held) => held,
            other => return Err(unexpected_answer("ListPanes", &other)),
        };

        // Both differences are settled before any pane is driven.
        let wanted: HashSet<PaneId> = panes.iter().copied().collect();
        for pane in &held {
            if !wanted.contains(&pane.pane_id) {
                let _ = self.ask(SupervisorRequestKind::Kill {
                    pane_id: pane.pane_id,
                    kill_policy: KillPolicy::Tree,
                });
            }
        }
        let kept: HashMap<PaneId, LivePane> = held
            .iter()
            .filter(|pane| wanted.contains(&pane.pane_id))
            .map(|pane| {
                (
                    pane.pane_id,
                    LivePane {
                        pid: pane.pid,
                        size: pane.size,
                    },
                )
            })
            .collect();
        for pane in panes.iter().filter(|pane| !kept.contains_key(pane)) {
            self.sink.exit(*pane, UNOBSERVED_EXIT);
        }
        *self.panes.lock().expect("supervisor panes") = kept;

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
        self.ask_done(SupervisorRequestKind::PauseOutput)
    }

    /// Put every held reader back to work: the supervisor writes what it held
    /// to this link and keeps writing.
    ///
    /// A refusal, a link that broke, and an answer that never came are all
    /// dropped here. Each one is a supervisor no longer serving this session,
    /// which every later request on this link reports in turn.
    pub fn resume_readers(&self) {
        let _ = self.ask(SupervisorRequestKind::ResumeOutput);
    }

    /// Wait until no byte this backend took for a child is still queued.
    ///
    /// [`write`](PtyBackend::write) sends the bytes to the supervisor and waits
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
    pub fn carried_panes(&self) -> Vec<CarriedPtyPane> {
        let panes = self.panes.lock().expect("supervisor panes");
        panes
            .iter()
            .map(|(pane_id, live)| CarriedPtyPane {
                pane_id: *pane_id,
                #[cfg(unix)]
                terminal_fd: None,
                pid: live.pid,
                size: live.size,
                exit: None,
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
    pub fn shut_down(&self) -> Result<(), PtyError> {
        match self.ask(SupervisorRequestKind::Shutdown) {
            Ok(SupervisorResult::Done) => Ok(()),
            Ok(other) => Err(unexpected_answer("Shutdown", &other)),
            Err(_) => Ok(()),
        }
    }

    /// Send one request and wait for the answer to that request, for at most
    /// the window [`answer_wait`] gives that request. The window starts once
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
    fn ask(&self, kind: SupervisorRequestKind) -> Result<SupervisorResult, PtyError> {
        let name = kind.name();
        let wait = answer_wait(&kind);
        let mut link = self.link.lock().expect("supervisor link");
        let deadline = Instant::now() + wait;
        let request_id = link.next_request_id;
        link.next_request_id += 1;
        link.writer
            .send(&SupervisorRequest { request_id, kind })
            .map_err(|error| PtyError::Io {
                detail: format!("{name} could not be sent to the supervisor: {error}"),
            })?;
        let response = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match link.answers.recv_timeout(left) {
                Ok(response) if response.request_id == Some(request_id) => break response,
                // The answer to an earlier request of this side's own, whose
                // wait already ran out.
                Ok(response) if response.request_id.is_some_and(|id| id < request_id) => {}
                Ok(response) => {
                    return Err(PtyError::Io {
                        detail: format!(
                            "the supervisor answered request {:?} while {name} \
                             (request {request_id}) was in flight",
                            response.request_id
                        ),
                    })
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(PtyError::Io {
                        detail: format!(
                            "the supervisor did not answer {name} within {} seconds",
                            wait.as_secs()
                        ),
                    })
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(PtyError::Io {
                        detail: format!("the supervisor link closed while {name} was in flight"),
                    })
                }
            }
        };
        drop(link);

        match response.result {
            MaybeKnown::Known(SupervisorResult::Error(payload)) => Err(PtyError::Io {
                detail: format!("the supervisor refused {name}: {}", payload.message),
            }),
            MaybeKnown::Known(result) => Ok(result),
            MaybeKnown::Unknown { name: answer } => Err(PtyError::Io {
                detail: format!(
                    "the supervisor answered {name} with {answer}, \
                     which this build has no name for"
                ),
            }),
        }
    }

    /// Send one request whose only good answer is
    /// [`SupervisorResult::Done`], and hand back nothing else.
    ///
    /// # Errors
    /// Returns whatever [`ask`](Self::ask) reports, and [`PtyError::Io`] naming
    /// both the request and an answer that is not `Done`.
    fn ask_done(&self, kind: SupervisorRequestKind) -> Result<(), PtyError> {
        let name = kind.name();
        match self.ask(kind)? {
            SupervisorResult::Done => Ok(()),
            other => Err(unexpected_answer(name, &other)),
        }
    }

    /// [`PtyError::UnknownPane`] when this backend does not drive `pane`.
    /// Checked before every request that names one pane.
    fn refuse_unknown_pane(&self, pane: PaneId) -> Result<(), PtyError> {
        if self
            .panes
            .lock()
            .expect("supervisor panes")
            .contains_key(&pane)
        {
            Ok(())
        } else {
            Err(PtyError::UnknownPane { pane })
        }
    }
}

impl PtyBackend for SupervisorPtyBackend {
    /// Ask the supervisor to open a pane: it makes a terminal of `size` and
    /// launches `spec` inside it.
    ///
    /// The child runs in the supervisor's process, not this one, and its output
    /// and exit arrive as events on the link. The returned handle is
    /// [`PtyHandle::detached`]: it carries no channels, and the caller starts
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
    fn spawn(
        &self,
        pane_id: PaneId,
        spec: SpawnSpec,
        size: PtySize,
    ) -> Result<PtyHandle, PtyError> {
        debug_assert!(
            !self
                .panes
                .lock()
                .expect("supervisor panes")
                .contains_key(&pane_id),
            "spawn into an already-live pane id {pane_id}; kill it before respawning"
        );
        let answer = self.ask(SupervisorRequestKind::Spawn {
            pane_id,
            spec,
            size,
        });
        let pid = match answer {
            Ok(SupervisorResult::Spawned { pid }) => pid,
            Ok(other) => return Err(unexpected_answer("Spawn", &other)),
            // Every failure of the exchange is reported as a spawn failure.
            Err(PtyError::Io { detail }) => return Err(PtyError::Spawn { detail }),
            Err(error) => return Err(error),
        };
        self.panes
            .lock()
            .expect("supervisor panes")
            .insert(pane_id, LivePane { pid, size });
        Ok(PtyHandle::detached(pane_id))
    }

    /// Retune a pane's terminal, which its child sees as a window-size change.
    ///
    /// The new size is recorded after the supervisor answers `Done`. A refused
    /// resize leaves the recorded size unchanged.
    ///
    /// # Errors
    /// Returns [`PtyError::UnknownPane`] when this backend does not drive
    /// `pane`, and [`PtyError::Io`] when the supervisor refuses or the link
    /// fails.
    fn resize(&self, pane: PaneId, size: PtySize) -> Result<(), PtyError> {
        self.refuse_unknown_pane(pane)?;
        self.ask_done(SupervisorRequestKind::Resize {
            pane_id: pane,
            size,
        })?;
        if let Some(live) = self.panes.lock().expect("supervisor panes").get_mut(&pane) {
            live.size = size;
        }
        Ok(())
    }

    /// Send bytes to a pane's child, which reach it as typed input.
    ///
    /// # Errors
    /// Returns [`PtyError::UnknownPane`] when this backend does not drive
    /// `pane`, and [`PtyError::Io`] when the supervisor refuses or the link
    /// fails.
    fn write(&self, pane: PaneId, bytes: &[u8]) -> Result<(), PtyError> {
        self.refuse_unknown_pane(pane)?;
        self.ask_done(SupervisorRequestKind::Write {
            pane_id: pane,
            bytes: bytes.to_vec(),
        })
    }

    /// End a pane's child according to `kill_policy` and drop the pane.
    ///
    /// The pane leaves this backend whatever the supervisor answers. No output
    /// and no exit for that pane reaches the sink afterwards.
    ///
    /// # Errors
    /// Returns [`PtyError::UnknownPane`] when this backend does not drive
    /// `pane`, and [`PtyError::Io`] when the supervisor refuses or the link
    /// fails.
    fn kill(&self, pane: PaneId, kill_policy: KillPolicy) -> Result<(), PtyError> {
        if self
            .panes
            .lock()
            .expect("supervisor panes")
            .remove(&pane)
            .is_none()
        {
            return Err(PtyError::UnknownPane { pane });
        }
        self.ask_done(SupervisorRequestKind::Kill {
            pane_id: pane,
            kill_policy,
        })
    }

    /// The live working directory of `pane`'s child, asked from the operating
    /// system by the supervisor, which is the child's parent. `None` when this
    /// backend does not drive `pane`, the pane has no live child, the platform
    /// has no lookup, the supervisor refuses, or the link fails.
    fn live_cwd(&self, pane: PaneId) -> Option<PathBuf> {
        self.refuse_unknown_pane(pane).ok()?;
        match self.ask(SupervisorRequestKind::LiveCwd { pane_id: pane }) {
            Ok(SupervisorResult::Cwd(cwd)) => cwd,
            _ => None,
        }
    }
}

/// How long `kind` waits for its answer: [`ANSWER_WAIT`], plus the grace window
/// of a kill that asks the child to exit on its own.
///
/// The supervisor answers such a kill after it has spent that window.
fn answer_wait(kind: &SupervisorRequestKind) -> Duration {
    match kind {
        SupervisorRequestKind::Kill {
            kill_policy: KillPolicy::Graceful { timeout } | KillPolicy::GracefulTree { timeout },
            ..
        } => ANSWER_WAIT + *timeout,
        _ => ANSWER_WAIT,
    }
}

/// The failure for an answer that does not fit the request it answers, naming
/// both. The answer is named by its variant alone; no payload reaches the
/// message.
fn unexpected_answer(request: &str, answer: &SupervisorResult) -> PtyError {
    PtyError::Io {
        detail: format!(
            "the supervisor answered {request} with {}",
            answer.wire_name()
        ),
    }
}

/// Start the thread that reads the link: it hands each answer to whoever is
/// waiting on [`Link::answers`] and each event to `sink`.
///
/// The thread ends when the link breaks, when a frame does not decode, or when
/// no one holds the receiving end of `answers`. Ending drops `answers`: a
/// caller waiting for an answer reads the link as closed. An event this build
/// has no name for is passed over, and the link keeps carrying the rest.
///
/// A pane whose chunk `sink` refused takes nothing more, its exit included;
/// every other pane keeps being delivered.
///
/// # Panics
/// Panics when the operating system cannot start the thread.
fn start_link_reader(
    mut reader: FrameReader,
    answers: Sender<SupervisorResponse<MaybeKnown<SupervisorResult>>>,
    sink: Arc<dyn PtySink>,
) {
    let _ = thread::Builder::new()
        .name("koshi-pty-link".to_string())
        .spawn(move || {
            // A pane whose chunk the consumer refused. Nothing more of that
            // pane is delivered, its exit included; every other pane keeps
            // being delivered.
            let mut refused: HashSet<PaneId> = HashSet::new();
            while let Ok(message) = reader.recv::<IncomingSupervisorMessage>() {
                match message {
                    SupervisorMessage::Response(response) => {
                        if answers.send(response).is_err() {
                            return;
                        }
                    }
                    SupervisorMessage::Event(MaybeKnown::Known(SupervisorEvent::Output {
                        pane_id,
                        bytes,
                    })) => {
                        if !refused.contains(&pane_id) && !sink.output(pane_id, bytes) {
                            refused.insert(pane_id);
                        }
                    }
                    SupervisorMessage::Event(MaybeKnown::Known(SupervisorEvent::Exited {
                        pane_id,
                        status,
                    })) => {
                        if !refused.contains(&pane_id) {
                            sink.exit(pane_id, status);
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
