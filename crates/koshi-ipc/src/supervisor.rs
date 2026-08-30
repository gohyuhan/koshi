//! The pane-supervisor protocol: how a session server drives the panes held by
//! a separate process.
//!
//! A session server keeps its panes in a helper process — the supervisor —
//! that keeps running while the session server replaces its own image. The
//! session server asks the supervisor to spawn, resize, write to and kill
//! panes; the supervisor sends back every byte those panes print and every
//! exit.
//!
//! One link carries both directions. A message from the session server is a
//! [`SupervisorRequest`](crate::supervisor::SupervisorRequest); a message from
//! the supervisor is a
//! [`SupervisorMessage`](crate::supervisor::SupervisorMessage), either the
//! [`SupervisorResponse`](crate::supervisor::SupervisorResponse) answering one
//! request or a [`SupervisorEvent`](crate::supervisor::SupervisorEvent) that
//! answers none. Framing is [`transport`](crate::transport), as in the session
//! and control-plane protocols.
//!
//! Every link opens with
//! [`Hello`](crate::supervisor::SupervisorRequestKind::Hello), checked by
//! [`SupervisorHandshake`](crate::supervisor::SupervisorHandshake). Its
//! versions run from
//! [`MIN_SUPERVISOR_PROTOCOL_VERSION`](crate::supervisor::MIN_SUPERVISOR_PROTOCOL_VERSION)
//! to
//! [`SUPERVISOR_PROTOCOL_VERSION`](crate::supervisor::SUPERVISOR_PROTOCOL_VERSION),
//! counted separately from the session protocol's
//! [`PROTOCOL_VERSION`](crate::protocol::PROTOCOL_VERSION) and the
//! control-plane protocol's
//! [`ROUTER_PROTOCOL_VERSION`](crate::router::ROUTER_PROTOCOL_VERSION).
//!
//! A supervisor keeps running the binary image it started from; the session
//! server that reconnects to it can be a newer build. A request kind the
//! supervisor does not have is refused by name and the link stays open.

use std::path::{Path, PathBuf};

use koshi_core::compat::SUPERVISOR_PROTOCOL;
use koshi_core::ids::{PaneId, SessionId};
use koshi_core::process::{ExitStatus, KillPolicy, PtySize, SpawnSpec};
use serde::{Deserialize, Serialize};

use crate::handshake::{GateWords, VersionGate};
use crate::protocol::{ConnectionToken, IpcErrorPayload};
use crate::wire::{Answer, Envelope, MaybeKnown, WireName, WireVariants};

/// The highest supervisor-link protocol version this build speaks, and the one
/// it uses when the peer speaks it too.
///
/// The value and the rule it follows live in
/// [`koshi_core::compat::SUPERVISOR_PROTOCOL`].
///
/// Version 1 is the first version of the link.
pub const SUPERVISOR_PROTOCOL_VERSION: u32 = SUPERVISOR_PROTOCOL.max;

/// The lowest supervisor-link protocol version this build speaks. A peer whose
/// highest is below this one is refused with
/// [`UnsupportedVersion`](crate::protocol::IpcErrorCode::UnsupportedVersion).
///
/// The floor is 1, the first version of the link. Raising it drops support
/// for every build below it.
pub const MIN_SUPERVISOR_PROTOCOL_VERSION: u32 = SUPERVISOR_PROTOCOL.min;

/// One message from a session server to its supervisor.
///
/// The envelope's own fields are fixed: decoding rejects any field it does not
/// know; a misspelled `request_id` is an error.
///
/// `K` is the request kind. A sender uses `SupervisorRequest`, where `K` is
/// [`SupervisorRequestKind`]. The supervisor uses
/// [`IncomingSupervisorRequest`], where a kind this build does not have
/// arrives as [`MaybeKnown::Unknown`].
pub type SupervisorRequest<K = SupervisorRequestKind> = Envelope<K>;

/// A supervisor request as the supervisor reads it: the kind may name
/// something this build does not have.
pub type IncomingSupervisorRequest = SupervisorRequest<MaybeKnown<SupervisorRequestKind>>;

/// What a supervisor request asks for.
///
/// A field this build does not know is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorRequestKind {
    /// Opens the link: names the range of supervisor-link protocol versions the
    /// session server speaks and presents the token that server started the
    /// supervisor with. Sent before any other kind.
    ///
    /// Sending it again on an open link is allowed: the versions and token are
    /// checked again, an accepted one settles the version again from its own
    /// range, and a refused one leaves the gate as it was.
    Hello {
        /// The lowest supervisor-link protocol version the session server
        /// speaks.
        min_protocol_version: u32,
        /// The highest supervisor-link protocol version the session server
        /// speaks.
        max_protocol_version: u32,
        /// The secret the session server started this supervisor with.
        token: ConnectionToken,
    },
    /// Open a pane: the supervisor makes a pseudo-terminal of `size` and
    /// launches `spec` inside it. Answered with
    /// [`Spawned`](SupervisorResult::Spawned).
    Spawn {
        /// The pane the supervisor keys this child by.
        pane_id: PaneId,
        /// What to launch.
        spec: SpawnSpec,
        /// The size the pane's terminal opens at.
        size: PtySize,
    },
    /// Retune an open pane's terminal, which its child sees as a window-size
    /// change.
    Resize {
        /// The pane to retune.
        pane_id: PaneId,
        /// The new size.
        size: PtySize,
    },
    /// Send bytes to a pane's child, which reach it as typed input.
    Write {
        /// The pane to write to.
        pane_id: PaneId,
        /// The bytes to write. They travel as one base64 string, the shape
        /// [`bytes`](crate::bytes) spells out.
        #[serde(with = "crate::bytes")]
        bytes: Vec<u8>,
    },
    /// End a pane's child and drop the pane. No output and no exit for that
    /// pane reaches the session server afterwards.
    Kill {
        /// The pane to close.
        pane_id: PaneId,
        /// How hard to end the child.
        kill_policy: KillPolicy,
    },
    /// Ask the operating system for the live working directory of a pane's
    /// child. Answered with [`Cwd`](SupervisorResult::Cwd).
    LiveCwd {
        /// The pane whose child to ask about.
        pane_id: PaneId,
    },
    /// List the panes the supervisor holds. A session server that has just
    /// replaced its own image asks this to find out what survived.
    ListPanes,
    /// Hold every pane's output and exit inside the supervisor instead of
    /// writing it to this link. Answered with [`Done`](SupervisorResult::Done),
    /// and no [`SupervisorEvent`] follows that answer here. Requests and their
    /// answers keep crossing the link.
    ///
    /// A session server about to replace its own process image sends it. The
    /// supervisor writes what it held to the next link.
    PauseOutput,
    /// Write every pane's output and exit to this link again, starting with
    /// what [`PauseOutput`](SupervisorRequestKind::PauseOutput) held. Answered
    /// with [`Done`](SupervisorResult::Done). A session server whose image swap
    /// was abandoned sends it.
    ResumeOutput,
    /// End the supervisor: it closes every pane it still holds and exits. The
    /// session server sends it when the session ends.
    Shutdown,
}

impl SupervisorRequestKind {
    /// The Hello this build opens a supervisor link with: the link versions it
    /// speaks, lowest first, and `token`, the secret the session server started
    /// the supervisor with. Every caller builds its Hello here.
    #[must_use]
    pub fn hello(token: ConnectionToken) -> SupervisorRequestKind {
        SupervisorRequestKind::Hello {
            min_protocol_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
            max_protocol_version: SUPERVISOR_PROTOCOL_VERSION,
            token,
        }
    }

    /// The kind's name, e.g. `"Spawn"`, with none of its payload: a Hello's
    /// token and a Write's bytes do not appear in it.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            SupervisorRequestKind::Hello { .. } => "Hello",
            SupervisorRequestKind::Spawn { .. } => "Spawn",
            SupervisorRequestKind::Resize { .. } => "Resize",
            SupervisorRequestKind::Write { .. } => "Write",
            SupervisorRequestKind::Kill { .. } => "Kill",
            SupervisorRequestKind::LiveCwd { .. } => "LiveCwd",
            SupervisorRequestKind::ListPanes => "ListPanes",
            SupervisorRequestKind::PauseOutput => "PauseOutput",
            SupervisorRequestKind::ResumeOutput => "ResumeOutput",
            SupervisorRequestKind::Shutdown => "Shutdown",
        }
    }
}

/// One message answering a [`SupervisorRequest`].
///
/// The envelope's own fields are fixed: decoding rejects any field it does not
/// know; a misspelled `request_id` is an error, not an absent one.
///
/// `R` is the answer. The supervisor uses `SupervisorResponse`, where `R` is
/// [`SupervisorResult`]. A session server reads it inside
/// [`IncomingSupervisorMessage`], where a result this build does not have
/// arrives as [`MaybeKnown::Unknown`].
pub type SupervisorResponse<R = SupervisorResult> = Answer<R>;

/// One pane the supervisor holds, as [`ListPanes`](SupervisorRequestKind::ListPanes)
/// reports it.
///
/// A field this build does not know is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorPane {
    /// The pane this record is for.
    pub pane_id: PaneId,
    /// The process id of the pane's child.
    pub pid: u32,
    /// The last size the pane's terminal was set to.
    pub size: PtySize,
}

/// The answer to a supervisor request.
///
/// A field this build does not know is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorResult {
    /// Answers [`SupervisorRequestKind::Hello`]: the link is open, the ranges
    /// overlap, and the token matched.
    Hello {
        /// The version both sides use on this link: the highest they both
        /// speak.
        protocol_version: u32,
    },
    /// Answers [`SupervisorRequestKind::Spawn`]: the pane is open and its
    /// child is running under this process id.
    Spawned {
        /// The process id of the new child.
        pid: u32,
    },
    /// Answers [`SupervisorRequestKind::ListPanes`]: one record per pane the
    /// supervisor holds.
    Panes(Vec<SupervisorPane>),
    /// Answers [`SupervisorRequestKind::LiveCwd`]: the child's working
    /// directory, and `None` when the operating system cannot answer or the
    /// directory's name is not valid UTF-8.
    ///
    /// A `PathBuf` that is not valid UTF-8 has no encoding on this wire, so a
    /// path such as `/tmp/\xff` travels as `None`.
    Cwd(Option<PathBuf>),
    /// Answers [`Resize`](SupervisorRequestKind::Resize),
    /// [`Write`](SupervisorRequestKind::Write),
    /// [`Kill`](SupervisorRequestKind::Kill),
    /// [`PauseOutput`](SupervisorRequestKind::PauseOutput),
    /// [`ResumeOutput`](SupervisorRequestKind::ResumeOutput) and
    /// [`Shutdown`](SupervisorRequestKind::Shutdown): the request was carried
    /// out and there is nothing to report back.
    Done,
    /// The request was refused.
    Error(IpcErrorPayload),
}

/// Something one pane did that no request asked about.
///
/// A field this build does not know is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorEvent {
    /// One chunk of a pane's child output, in the order the pane produced it.
    Output {
        /// The pane that printed these bytes.
        pane_id: PaneId,
        /// The bytes themselves. They travel as one base64 string, the shape
        /// [`bytes`](crate::bytes) spells out.
        #[serde(with = "crate::bytes")]
        bytes: Vec<u8>,
    },
    /// A pane's child ended. It comes after the last
    /// [`Output`](SupervisorEvent::Output) for that pane.
    Exited {
        /// The pane whose child ended.
        pane_id: PaneId,
        /// How it ended.
        status: ExitStatus,
    },
}

impl SupervisorEvent {
    /// The event's name, e.g. `"Output"`, with none of its payload: an
    /// Output's bytes do not appear in it.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            SupervisorEvent::Output { .. } => "Output",
            SupervisorEvent::Exited { .. } => "Exited",
        }
    }
}

/// One message from a supervisor to its session server: the answer to a
/// request, or an event that answers none.
///
/// Every frame the supervisor sends is one of these two.
///
/// `R` and `E` are the answer and the event. The supervisor uses
/// `SupervisorMessage`, where they are [`SupervisorResult`] and
/// [`SupervisorEvent`]. A session server uses [`IncomingSupervisorMessage`],
/// where a variant this build does not have arrives as
/// [`MaybeKnown::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorMessage<R = SupervisorResult, E = SupervisorEvent> {
    /// The answer to one request the session server sent.
    Response(SupervisorResponse<R>),
    /// Something a pane did that no request asked about.
    Event(E),
}

/// A supervisor message as a session server reads it: the answer or the event
/// may name something this build does not have.
pub type IncomingSupervisorMessage =
    SupervisorMessage<MaybeKnown<SupervisorResult>, MaybeKnown<SupervisorEvent>>;

/// What the supervisor-link protocol's gate calls itself, and the versions it
/// speaks.
const SUPERVISOR_WORDS: GateWords = GateWords {
    peer: "supervisor",
    token_owner: "the supervisor's",
    caller: "session server",
    versions: "supervisor-link protocol versions",
    channel: "link",
    min_version: MIN_SUPERVISOR_PROTOCOL_VERSION,
    max_version: SUPERVISOR_PROTOCOL_VERSION,
};

/// One supervisor link's handshake gate, held by the supervisor for the link's
/// lifetime. Starts closed; a [`SupervisorRequestKind::Hello`] whose version
/// range overlaps this build's and whose token matches opens it, and every
/// other request kind is served only while it is open. The check itself is the
/// gate every koshi protocol shares.
#[derive(Debug)]
pub struct SupervisorHandshake(VersionGate);

impl SupervisorHandshake {
    /// A gate for one newly accepted supervisor link, closed until a Hello
    /// opens it.
    #[must_use]
    pub fn new(expected: ConnectionToken) -> SupervisorHandshake {
        SupervisorHandshake(VersionGate::new(expected, SUPERVISOR_WORDS))
    }

    /// The link protocol version this link settled on, or `None` while no
    /// Hello has been accepted.
    ///
    /// The supervisor puts it in [`SupervisorResult::Hello`].
    #[must_use]
    pub fn agreed(&self) -> Option<u32> {
        self.0.agreed()
    }

    /// The refusal for a request kind this build does not have, named `name`.
    ///
    /// A closed gate answers
    /// [`HelloRequired`](crate::protocol::IpcErrorCode::HelloRequired), the
    /// same as any other kind arriving before a Hello. An open gate answers
    /// [`UnsupportedKind`](crate::protocol::IpcErrorCode::UnsupportedKind)
    /// naming it, and the link keeps serving.
    #[must_use]
    pub fn refuse_unknown(&self, name: &str) -> IpcErrorPayload {
        self.0.refuse_unknown(name)
    }

    /// Check one incoming request kind against the link's state.
    ///
    /// A [`Hello`](SupervisorRequestKind::Hello) is checked version first, then
    /// token: a session server whose version range does not overlap
    /// [`MIN_SUPERVISOR_PROTOCOL_VERSION`]`..=`[`SUPERVISOR_PROTOCOL_VERSION`]
    /// is refused as
    /// [`UnsupportedVersion`](crate::protocol::IpcErrorCode::UnsupportedVersion)
    /// with both ranges named, a token that does not equal the supervisor's is
    /// refused as [`BadToken`](crate::protocol::IpcErrorCode::BadToken), and a
    /// Hello passing both checks settles the link's version and opens the gate.
    /// Any other kind is accepted while the gate is open and refused as
    /// [`HelloRequired`](crate::protocol::IpcErrorCode::HelloRequired) while it
    /// is not.
    ///
    /// `Ok(())` means the caller serves the request — a Hello is answered with
    /// [`SupervisorResult::Hello`] carrying [`agreed`](Self::agreed). An `Err`
    /// carries the refusal to send back, and the gate keeps the state it had.
    pub fn check(&mut self, kind: &SupervisorRequestKind) -> Result<(), IpcErrorPayload> {
        match kind {
            SupervisorRequestKind::Hello {
                min_protocol_version,
                max_protocol_version,
                token,
            } => self
                .0
                .hello(*min_protocol_version, *max_protocol_version, token),
            other => self.0.other(other.name()),
        }
    }
}

/// The address the supervisor holding `session`'s panes listens on: the string
/// [`Connection::connect`](crate::transport::Connection::connect) takes.
///
/// `supervisor_pid` is the process id of that supervisor and is part of the
/// address: two supervisors of one session with different process ids listen
/// at different addresses.
///
/// On Unix this is a socket-file path, `session-<uuid>-pty-<pid>.sock` directly
/// inside `runtime_dir`. On Windows it is the pipe name
/// `koshi-pty-session-<uuid>-<pid>`, and `runtime_dir` goes unused. Callers
/// resolve `runtime_dir` through `koshi_paths::runtime_dir()`.
#[must_use]
pub fn supervisor_socket_addr(
    runtime_dir: &Path,
    session: SessionId,
    supervisor_pid: u32,
) -> String {
    #[cfg(unix)]
    {
        runtime_dir
            .join(format!("{session}-pty-{supervisor_pid}.sock"))
            .display()
            .to_string()
    }
    #[cfg(windows)]
    {
        let _ = runtime_dir;
        format!("koshi-pty-{session}-{supervisor_pid}")
    }
}

impl WireVariants for SupervisorRequestKind {
    /// Every supervisor request kind this build has: one entry per variant of
    /// [`SupervisorRequestKind`], spelled as [`SupervisorRequestKind::name`]
    /// spells it.
    const VARIANTS: &'static [&'static str] = &[
        "Hello",
        "Spawn",
        "Resize",
        "Write",
        "Kill",
        "LiveCwd",
        "ListPanes",
        "PauseOutput",
        "ResumeOutput",
        "Shutdown",
    ];
}

impl WireName for SupervisorRequestKind {
    fn wire_name(&self) -> &'static str {
        self.name()
    }
}

impl WireVariants for SupervisorResult {
    /// Every supervisor answer this build has: one entry per variant of
    /// [`SupervisorResult`], spelled as its `wire_name` spells it.
    const VARIANTS: &'static [&'static str] =
        &["Hello", "Spawned", "Panes", "Cwd", "Done", "Error"];
}

impl WireName for SupervisorResult {
    fn wire_name(&self) -> &'static str {
        match self {
            SupervisorResult::Hello { .. } => "Hello",
            SupervisorResult::Spawned { .. } => "Spawned",
            SupervisorResult::Panes(_) => "Panes",
            SupervisorResult::Cwd(_) => "Cwd",
            SupervisorResult::Done => "Done",
            SupervisorResult::Error(_) => "Error",
        }
    }
}

impl WireVariants for SupervisorEvent {
    /// Every supervisor event this build has: one entry per variant of
    /// [`SupervisorEvent`], spelled as [`SupervisorEvent::name`] spells it.
    const VARIANTS: &'static [&'static str] = &["Output", "Exited"];
}

impl WireName for SupervisorEvent {
    fn wire_name(&self) -> &'static str {
        self.name()
    }
}

#[cfg(test)]
mod tests;
