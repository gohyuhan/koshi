//! Driving panes that live in another process.
//!
//! A pseudoconsole — the Windows kernel object behind a pane's terminal —
//! cannot be handed to another process, so the process that opens one must
//! never exit. A session server that replaces its own image keeps its panes in
//! a helper process, the supervisor, and drives them over a link.
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

/// The exit reported for a pane the supervisor no longer holds: the child ended
/// with no session server left to take its status, so this one observed none.
///
/// The same value a Unix pane taken back through `PortablePtyBackend::adopt`
/// reports for a child that cannot be waited on.
const UNOBSERVED_EXIT: ExitStatus = ExitStatus::ExitCode(-1);

/// How long one request waits for its answer before the exchange is reported as
/// failed.
///
/// The supervisor serves one link at a time, so this window also covers the
/// time it is still spending on the link a replaced process image left behind.
const ANSWER_WAIT: Duration = Duration::from_secs(10);

/// What this side keeps for one pane the supervisor holds.
///
/// The supervisor is the authority on the pane itself. These two facts are kept
/// here so [`carried_panes`](SupervisorPtyBackend::carried_panes) answers
/// without a round trip.
#[derive(Debug, Clone, Copy)]
struct LivePane {
    /// The process id of the pane's child, as the supervisor reported it.
    pid: u32,
    /// The last size this pane's terminal was set to: what it was spawned or
    /// taken back at, then whatever the newest successful
    /// [`resize`](PtyBackend::resize) carried.
    size: PtySize,
}

/// The link, held under one lock so one request is in flight at a time.
///
/// The lock is taken from writing a request to reading its answer, so the
/// answer a caller reads is always the one to its own request.
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
/// other, so this backend's own process can exit and be replaced while every
/// pane keeps running. A replacement image calls [`connect`](Self::connect)
/// again and drives the same panes.
///
/// One process builds one of these and keeps it. Dropping it leaves the link's
/// reader thread holding the reading half, so the link closes when this process
/// exits or when [`shut_down`](Self::shut_down) ends the supervisor.
pub struct SupervisorPtyBackend {
    /// The link to the supervisor.
    link: Mutex<Link>,
    /// The panes this backend believes the supervisor holds, keyed by id. A
    /// [`spawn`](PtyBackend::spawn) adds one and a [`kill`](PtyBackend::kill)
    /// removes one.
    panes: Mutex<HashMap<PaneId, LivePane>>,
    /// Where every pane's output and exit is delivered. Held so
    /// [`connect`](Self::connect) can report a pane the supervisor no longer
    /// has as ended.
    sink: Arc<dyn PtySink>,
}

impl SupervisorPtyBackend {
    /// Open a link to the supervisor listening at `addr`, present `token`, and
    /// settle which panes this backend drives.
    ///
    /// `panes` is what the caller believes is running — empty for a session
    /// starting fresh, and the carried pane list for one that has just replaced
    /// its own image. It is settled against what the supervisor actually holds:
    ///
    /// - A pane in `panes` the supervisor does not hold is reported to `sink`
    ///   as ended, carrying [`ExitStatus::ExitCode`]`(-1)`: the status a child
    ///   that cannot be waited on reports.
    /// - A pane the supervisor holds that is not in `panes` is killed with
    ///   [`KillPolicy::Tree`].
    ///
    /// Every remaining pane is driven by the returned backend.
    ///
    /// # Errors
    /// Returns [`PtyError::Io`] when the link cannot be opened, when the
    /// supervisor does not answer within the answer wait, when it refuses the
    /// Hello, or when it cannot list its panes.
    pub fn connect(
        addr: &str,
        token: ConnectionToken,
        sink: Arc<dyn PtySink>,
        panes: &[PaneId],
    ) -> Result<SupervisorPtyBackend, PtyError> {
        let connection = Connection::connect(addr).map_err(|error| PtyError::Io {
            detail: format!("the supervisor at {addr} could not be reached: {error}"),
        })?;
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

        match backend.ask(SupervisorRequestKind::hello(token))? {
            SupervisorResult::Hello { .. } => {}
            other => return Err(unexpected_answer("Hello", &other)),
        }
        let held = match backend.ask(SupervisorRequestKind::ListPanes)? {
            SupervisorResult::Panes(held) => held,
            other => return Err(unexpected_answer("ListPanes", &other)),
        };

        // The supervisor's answer is the authority on what is still running,
        // so both differences are settled before any pane is driven.
        let wanted: HashSet<PaneId> = panes.iter().copied().collect();
        for pane in &held {
            if !wanted.contains(&pane.pane_id) {
                let _ = backend.ask(SupervisorRequestKind::Kill {
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
            backend.sink.exit(*pane, UNOBSERVED_EXIT);
        }
        *backend.panes.lock().expect("supervisor panes") = kept;

        Ok(backend)
    }

    /// Hold every pane's reader still, so nothing is read from a terminal
    /// without being handed to the consumer.
    ///
    /// Every pane's reader lives inside the supervisor, so the supervisor takes
    /// the hold: it stops writing pane events to this link, and requests and
    /// their answers keep crossing it. The link's one reader thread hands every
    /// frame to the sink before it reads the next, so by the time this returns
    /// `Ok(())` the consumer holds everything the supervisor wrote. What the
    /// panes print from then on waits inside the supervisor and reaches the next
    /// link.
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
    /// A refusal, a link that broke, and an answer that never came are all a
    /// supervisor no longer serving this session, which every later request on
    /// this link reports in turn.
    pub fn resume_readers(&self) {
        let _ = self.ask(SupervisorRequestKind::ResumeOutput);
    }

    /// Wait until no byte this backend took for a child is still queued.
    ///
    /// [`write`](PtyBackend::write) sends the bytes to the supervisor and waits
    /// for its answer, so a write that has returned is already the supervisor's
    /// and this process queues nothing. The supervisor keeps running across an
    /// image swap, and its own writer threads carry those bytes to the
    /// terminals.
    ///
    /// # Errors
    /// Never returns an error. The signature matches
    /// [`PortablePtyBackend::flush_writers`](crate::portable::PortablePtyBackend::flush_writers),
    /// which the swap calls the same way on both platforms.
    pub fn flush_writers(&self) -> Result<(), PtyError> {
        Ok(())
    }

    /// One record per live pane: what a new process image needs to take each
    /// pane back.
    ///
    /// The pane itself never moves — the supervisor keeps holding it — so the
    /// record carries only the pane's identity, its child's process id and its
    /// size. The terminal descriptor is always `None`, because that descriptor
    /// belongs to the supervisor, and the exit status is always `None`, because
    /// the supervisor reaps every child and reports the status over the link.
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
    /// as success: the supervisor is gone or is no longer answering, and its
    /// own idle window ends it either way.
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
    /// the window [`answer_wait`] gives that request.
    ///
    /// The link lock is held for the whole exchange, so two callers never read
    /// each other's answers. An answer to a request that already ran out of
    /// time arrives with that request's id and is passed over here, so one
    /// exchange running out of time leaves the link in step.
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
        let deadline = Instant::now() + wait;
        let mut link = self.link.lock().expect("supervisor link");
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
                Ok(response) if matches!(response.request_id, Some(id) if id < request_id) => {}
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
    /// and exit arrive as events on the link. The returned handle carries no
    /// channels, so the caller starts no relay thread for the pane.
    ///
    /// # Errors
    /// Returns [`PtyError::Spawn`] when the supervisor cannot open the
    /// terminal or launch the child, and [`PtyError::Io`] when the link
    /// fails.
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
            // A pane that could not be opened is a spawn failure on either side
            // of the link.
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
    /// The new size is recorded only once the supervisor took it, so a carried
    /// pane names the size its child was actually told.
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
    /// system by the supervisor, which is the child's parent. `None` when the
    /// pane has no live child, the platform has no lookup, or the link fails.
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
/// The supervisor answers such a kill only after it has spent that window, so
/// the wait carries it.
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
/// both. The answer is named by its variant alone, so no payload — which can
/// hold a child's output — reaches the message.
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
/// The thread ends when the link breaks, which drops `answers` and releases a
/// caller waiting for an answer that will never come. An event this build has
/// no name for is passed over, and the link keeps carrying the rest.
fn start_link_reader(
    mut reader: FrameReader,
    answers: Sender<SupervisorResponse<MaybeKnown<SupervisorResult>>>,
    sink: Arc<dyn PtySink>,
) {
    let _ = thread::Builder::new()
        .name("koshi-pty-link".to_string())
        .spawn(move || {
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
                        // A sink that refuses a chunk has lost the runtime
                        // behind it, so nothing more can be delivered at all.
                        if !sink.output(pane_id, bytes) {
                            return;
                        }
                    }
                    SupervisorMessage::Event(MaybeKnown::Known(SupervisorEvent::Exited {
                        pane_id,
                        status,
                    })) => sink.exit(pane_id, status),
                    SupervisorMessage::Event(MaybeKnown::Unknown { .. }) => {}
                }
            }
        })
        .expect("spawn supervisor link reader thread");
}

#[cfg(test)]
mod tests;
