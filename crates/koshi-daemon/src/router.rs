//! The router process: one per user, owning the list of running sessions.
//!
//! [`run_router`](crate::router::run_router) takes the advisory lock on the
//! router lock file, binds the router's control socket, writes the endpoint
//! file advertising it, asks every session already running to describe
//! itself, and then serves control-plane requests until no session is left.
//!
//! The router is the parent of every session server it starts. It hands a
//! caller a session's control-socket address and steps out: pane traffic runs
//! between the caller and that session server directly, never through here.
//!
//! One thread accepts connections, serves only this user's own, and gives each
//! its own serving thread. A serving thread holds only a channel sender: the
//! session list has one owner, the dispatcher loop on the main thread.
//!
//! The dispatcher never waits on a session. Every connection to a session
//! runs on a thread of its own and reports back as an event: a probe that
//! connects and closes, a description that asks the session for its overview,
//! and the read of a new session server's ready line. A request that needs one
//! of those answers waits in `RouterSessions` with a deadline, and the
//! dispatcher serves every other event meanwhile.
//!
//! A session that dies leaves the list four ways: its reaper thread reports
//! the exit of a session server this router started; on Unix, a watcher thread
//! reports the exit of a session registered from its endpoint file whose
//! server is a child of this process; a probe every 30 seconds finds nothing
//! listening at the address of a session no thread watches; or a lookup's
//! probe finds nothing listening at its address. A reported exit removes the
//! session only when its probe finds nothing listening. Example: on Windows, a
//! session that replaces its image serves on from a new process at the same
//! address after its old process exits, and it stays listed. Each removal
//! takes the entry and, for a session this user started, the files it left
//! behind, the socket file it bound in the shared directory included. On
//! macOS a refused connect also comes from a session whose listen queue is
//! full: a session whose process still runs as the one that wrote its endpoint
//! file is kept, and a probe or a description connects again for up to 1
//! second while that process runs.
//!
//! The address probed is the one the session's endpoint file names at that
//! moment: a session that restarts can bind a new address, such as the shared
//! directory once `allow-other-users` is on. A probe that found nothing
//! listening removes the session only when the endpoint file still holds what
//! the probe read: every bind writes a new connection token. A session whose
//! endpoint file exists and cannot be read is never removed.
//!
//! A session the list does not hold yet, this user's or one another local
//! user started, is asked to describe itself when it is found: at startup and
//! on every lookup. A lookup by an id the list holds reads no folder of the
//! shared directory. Neither does a lookup by a name that a session of this
//! user's carries. A lookup by another id reads only that id's path in each
//! user's folder, and asks no session of another user's but the one it names.
//! A session of this user's that is between two process images is recorded as
//! restarting and is not asked. At most 16 descriptions of other users'
//! sessions run at once, and each description ends 5 seconds after its
//! session was asked. A lookup waits up to 5 seconds for the answer. A remote
//! listing of the sessions waits for none. An answer that arrives after the
//! wait registers the session then. An answer that a thread sent before a
//! wait ended is applied before that wait ends.
//!
//! A lookup by a name that matches one session another local user started is
//! refused while another session did not answer, or while a directory of
//! sessions could not be read. The refusal reads ``cannot tell whether
//! `quiet-lake` is unique (1 running session did not answer)``. A session id
//! that two sockets in the shared directory advertise is never asked, and a
//! lookup of it is refused naming their owners.
//!
//! A restart comes due in two ways: the router writes the `Restarting` reply
//! to a `Restart` request, or a check of its program file finds another koshi
//! version. Each connection the router or its remote listener accepts runs
//! that check, as `check_router_executable_file` states, and so does each
//! Hello refused for its protocol version. Once a restart is due, the router
//! lets every request already waiting finish, a starting session server
//! included, then restarts into its program file. Until then it answers every
//! request that needs no wait, and refuses every new one that would wait with
//! [`ROUTER_RESTARTING_MESSAGE`](koshi_ipc::router::ROUTER_RESTARTING_MESSAGE).
//! On Unix it removes its program file and replaces its own running image, and
//! keeps the same process id. On Windows it starts the new koshi, which waits
//! for the router lock, and then runs its own shutdown and exits. A restart
//! that fails resumes serving, still ignoring the SIGPIPE signal on Unix, and
//! the first check 30 seconds or more after the failure reads the program file
//! again. Every serving thread blocks
//! SIGPIPE on its own mask; a write to a peer that hung up returns an error in
//! every disposition state.
//!
//! With no session left, the dispatcher waits one idle window for a request
//! and exits when none arrives. A caller that needs the router again starts
//! it: connect, and on failure spawn the router and retry.
//!
//! The router also opens the machine's TLS port for remote clients, when
//! `koshi.kdl` names an address and the operator has switched remote access
//! on. The remote listener holds those connections and asks the dispatcher
//! what each caller's secret reaches. The dispatcher keeps the socket of every
//! connection it admitted: a revoked or replaced secret ends its connections
//! at once.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use fs4::{FileExt, TryLockError};
use koshi_config::layer::merge_server;
use koshi_config::types::ServerConfig;
use koshi_core::discovery::SessionOverview;
use koshi_core::ids::SessionId;
use koshi_core::naming::{generate_name, NameKind};
use koshi_ipc::endpoint::{
    compute_socket_address, is_refusal_from_live_session, is_replacing_its_image,
    remove_socket_file, resolve_resume_file_path, EndpointFile, ServerProgramFile,
};
#[cfg(windows)]
use koshi_ipc::endpoint::{remove_advertisement_marker, resolve_advertisement_marker_path};
use koshi_ipc::error::{IpcError, RemoteFile};
use koshi_ipc::plane::{self, RequestDisposition};
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::remote_migration::migrate_remote_listener_files;
use koshi_ipc::remote_state::{
    is_remote_access_enabled, CertificateFile, RemoteAccessRecord, CERTIFICATE_FILE_FORMAT,
    REMOTE_ACCESS_RECORD_FILE_FORMAT,
};
use koshi_ipc::remote_tokens::{
    hash_connection_token, resolve_token_store_path, TokenScope, TokenStore,
};
use koshi_ipc::remote_wire::RemoteSessionRow;
use koshi_ipc::router::{
    compute_router_socket_address, resolve_router_endpoint_path, resolve_router_lock_path,
    resolve_router_program_file_path, ControlPlane, RouterHandshake, RouterRequestKind,
    RouterResponse, RouterResult, SessionAddress, SessionSelector, SessionServerReady,
    ROUTER_PROTOCOL_VERSION, ROUTER_RESTARTING_MESSAGE,
};
use koshi_ipc::tls;
use koshi_ipc::transport::{self, Connection, Listener};
use koshi_ipc::validate::{reclaim_stale_socket, validate_socket_address};

use koshi_link::discovery::repeat_while_live_session_refuses;
use koshi_link::error::CliError;
use koshi_link::ipc_client::{self, DuplicatedForeignSession, ForeignSessionListing, UnreadPath};
use koshi_link::router_client::{ROUTER_SUBCOMMAND, RUNTIME_DIRECTORY_FLAG};
use koshi_runtime::executable_watch::ExecutableWatch;
use koshi_runtime::server::is_binary_runnable;

use crate::process;
use crate::remote_listener::{
    self, AdmissionAsk, LocateRefusal, RemoteConnectionAdmission, WarningRateLimiter,
};
use crate::session_server::{ALLOW_OTHER_USERS_FLAG, SESSION_SERVER_SUBCOMMAND};

#[cfg(test)]
mod tests;

/// The version of the binary this router is, reported in its Hello answer.
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the dispatcher waits for a request while no session is running.
/// A window that passes with the list still empty ends the router.
const ROUTER_IDLE_TIMEOUT_DURATION: Duration = Duration::from_secs(30);

/// How long a newly started session server has to report the address it
/// bound. A slower start is treated as a failed start.
const SESSION_SERVER_READY_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

/// How long a session has to describe itself, counted from the moment it is
/// asked, and how long a lookup or a remote attach waits for the descriptions
/// it started. A request never waits on one description past this long after
/// that session was asked.
const SESSION_DISCOVERY_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// How often the dispatcher probes the sessions no thread watches, through
/// [`start_unwatched_session_probes`]. While every listed session has an exit
/// watcher, nothing is probed.
const SESSION_LIVENESS_CHECK_INTERVAL_DURATION: Duration = Duration::from_secs(30);

/// The most descriptions of sessions other local users started that run at
/// once. A session asked past it is not asked, and records
/// [`UnansweredSessionReason::TooManyOtherUserDescriptions`].
const MAX_OTHER_USER_DESCRIPTIONS_IN_FLIGHT: usize = 16;

/// How long the accept loop pauses after a failed accept before trying
/// again.
const ACCEPT_RETRY_DELAY_DURATION: Duration = Duration::from_millis(100);

/// How long a replacement router waits for the previous router to release the
/// router lock. The operating system releases that lock if the previous router
/// dies.
const LOCK_HANDOVER_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

/// How long the lock wait pauses between attempts on the router lock.
const LOCK_HANDOVER_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(100);

/// How long shutdown pauses after withdrawing the socket. A serving thread
/// that is writing a reply has this long to finish it.
const DRAIN_GRACE_DURATION: Duration = Duration::from_millis(100);

/// The flag carrying a `--profile` name to the session server the router
/// starts. The session opens that profile's tabs and panes.
const PROFILE_FLAG: &str = "--profile";

/// The flag this router passes to the router it starts, telling that one to
/// wait for the router lock rather than yield to the router holding it.
const WAIT_FOR_LOCK_FLAG: &str = "--wait-for-lock";

/// The Win32 `CREATE_NO_WINDOW` creation flag: the started process gets a
/// console with no window on screen.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The running sessions, keyed by id. Owned by the dispatcher loop alone.
type SessionRegistry = HashMap<SessionId, SessionRecord>;

/// What the router knows about one running session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRecord {
    /// The session's generated display name.
    session_name: String,
    /// The session's control-socket address: a socket-file path on Unix, a
    /// bare pipe name on Windows.
    socket_address: String,
    /// The process id of the session server serving that socket, and `0` for
    /// a session another local user started, whose process this router does
    /// not own.
    process_id: u32,
    /// Whether the exit of the session server's process still reaches the
    /// dispatcher: a thread waits on the process and reports its exit as
    /// [`RouterEvent::ChildExited`] or, on Unix,
    /// `RouterEvent::ChildProcessReaped`. The dispatcher sets it to `false`
    /// when it takes that report. A session with none is probed every
    /// [`SESSION_LIVENESS_CHECK_INTERVAL_DURATION`].
    has_exit_watcher: bool,
}

/// What the dispatcher holds about sessions: the list, the connections to
/// sessions it started and has not heard back from, and the requests waiting
/// on them. Owned by the dispatcher loop alone. A thread that connects to a
/// session reads files and sockets only, and reports back as a
/// [`RouterEvent`].
struct RouterSessions {
    /// The running sessions the router knows.
    session_registry: SessionRegistry,
    /// Each description [`start_session_description`] started whose answer
    /// is not applied, by the session asked. A session here is not asked
    /// again.
    description_in_flight_by_session_id: BTreeMap<SessionId, DescriptionInFlight>,
    /// The listed sessions a probe thread is connecting to. A session here is
    /// not probed again until its [`RouterEvent::SessionProbed`] arrives.
    probing_session_ids: BTreeSet<SessionId>,
    /// Why each advertised session left out of the list stayed out, from its
    /// last description or the last read of its endpoint file. Asking the
    /// session again clears its entry.
    unanswered_reason_by_session_id: BTreeMap<SessionId, UnansweredSessionReason>,
    /// The attach lookups waiting for a description or a probe, in arrival
    /// order.
    waiting_attach_lookups: Vec<WaitingAttachLookup>,
    /// The remote attaches waiting for a description, in arrival order.
    waiting_remote_locates: Vec<WaitingRemoteLocate>,
    /// The session creations waiting to start their session server, in
    /// arrival order.
    queued_session_creations: Vec<QueuedSessionCreation>,
    /// The session servers started and not yet reported, in start order.
    starting_session_servers: Vec<StartingSessionServer>,
    /// Whether a [`RouterEvent::RestartDue`] arrived and the restart waits for
    /// the waiting requests to finish.
    is_restart_pending: bool,
    /// The children an earlier image of this process started that a thread of
    /// this image waits on, from [`adopt_inherited_children`] until their
    /// [`RouterEvent::ChildProcessReaped`] arrives.
    #[cfg(unix)]
    inherited_child_process_ids: BTreeSet<u32>,
    /// The children of this process that no thread waits on, because the
    /// waiting thread could not start. [`reap_unwaited_children`] reaps each
    /// one once it has exited.
    #[cfg(unix)]
    unwaited_child_process_ids: BTreeSet<u32>,
}

impl RouterSessions {
    /// The sessions `session_registry` holds, with nothing asked and nothing
    /// waiting.
    fn from_session_registry(session_registry: SessionRegistry) -> RouterSessions {
        RouterSessions {
            session_registry,
            description_in_flight_by_session_id: BTreeMap::new(),
            probing_session_ids: BTreeSet::new(),
            unanswered_reason_by_session_id: BTreeMap::new(),
            waiting_attach_lookups: Vec::new(),
            waiting_remote_locates: Vec::new(),
            queued_session_creations: Vec::new(),
            starting_session_servers: Vec::new(),
            is_restart_pending: false,
            #[cfg(unix)]
            inherited_child_process_ids: BTreeSet::new(),
            #[cfg(unix)]
            unwaited_child_process_ids: BTreeSet::new(),
        }
    }

    /// Whether a request waits: an attach lookup, a remote attach, a queued
    /// creation, or a starting session server.
    fn has_waiting_request(&self) -> bool {
        !self.waiting_attach_lookups.is_empty()
            || !self.waiting_remote_locates.is_empty()
            || !self.queued_session_creations.is_empty()
            || !self.starting_session_servers.is_empty()
    }
}

/// A description [`start_session_description`] started whose answer the
/// dispatcher has not applied.
struct DescriptionInFlight {
    /// When the session was asked.
    asked_at: Instant,
    /// Whether the session is one another local user started.
    is_other_user_session: bool,
    /// Set by the describing thread just before it sends its answer.
    is_answer_sent: Arc<AtomicBool>,
}

impl DescriptionInFlight {
    /// The moment a request waiting for this description is looked at again,
    /// while the description is still due at `now`: `now` once its answer is
    /// sent, and [`SESSION_DISCOVERY_TIMEOUT_DURATION`] after the session was
    /// asked before that. `None` once that moment is reached with no answer
    /// sent.
    fn find_due_at(&self, now: Instant) -> Option<Instant> {
        if self.is_answer_sent.load(Ordering::SeqCst) {
            return Some(now);
        }
        let due_at = self.asked_at + SESSION_DISCOVERY_TIMEOUT_DURATION;
        (due_at > now).then_some(due_at)
    }
}

/// An attach lookup waiting for the descriptions it started, or for the probe
/// of the session it found.
struct WaitingAttachLookup {
    /// The session the caller named.
    session_selector: SessionSelector,
    /// The advertised sessions found unlisted when the lookup arrived whose
    /// answers can change its answer: for a [`SessionSelector::SessionId`],
    /// that session alone, when it was among them; for a
    /// [`SessionSelector::SessionName`], all of them. Each was asked to
    /// describe itself, or has a reason recorded, such as an endpoint file that
    /// cannot be read.
    awaited_session_ids: BTreeSet<SessionId>,
    /// How many sessions the shared directory held past its caps when the
    /// lookup arrived, as
    /// [`ForeignSessionListing::unlisted_session_count`](ipc_client::ForeignSessionListing::unlisted_session_count)
    /// counts them. Each counts as a session that did not answer. Always `0`
    /// for a [`SessionSelector::SessionId`].
    unlisted_session_count: usize,
    /// Each path a listing of sessions could not read when the lookup arrived,
    /// each once: `runtime_directory` when this user's sessions cannot be
    /// listed, and the
    /// [`ForeignSessionListing::unread_path`](ipc_client::ForeignSessionListing::unread_path)
    /// of the shared listing. The sessions under each one are unknown.
    unread_paths: Vec<UnreadPath>,
    /// [`SESSION_DISCOVERY_TIMEOUT_DURATION`] after the lookup arrived. The
    /// lookup waits for no description past it.
    answer_deadline: Instant,
    /// The listed session the selector named, while the probe the lookup
    /// waits for runs. `None` before the selector names a listed session.
    probed_session_id: Option<SessionId>,
    /// Where the answer goes.
    response_sender: Sender<RouterResult>,
}

/// A remote attach waiting for the descriptions it started.
struct WaitingRemoteLocate {
    /// What the caller's secret reaches.
    scope: TokenScope,
    /// The number the caller's connection is registered under.
    remote_connection_id: u64,
    /// The session the caller named.
    session_selector: SessionSelector,
    /// Every advertised session of this user's found unlisted when the request
    /// arrived, whatever the selector names.
    awaited_session_ids: BTreeSet<SessionId>,
    /// [`SESSION_DISCOVERY_TIMEOUT_DURATION`] after the request arrived.
    answer_deadline: Instant,
    /// Where the endpoint file path, or
    /// [`LocateRefusal::NotReached`], goes.
    response_sender: Sender<Result<PathBuf, LocateRefusal>>,
}

/// A session creation waiting to start its session server. It starts its
/// session server once no description asked less than
/// [`SESSION_DISCOVERY_TIMEOUT_DURATION`] ago is unanswered.
struct QueuedSessionCreation {
    /// The `--profile` name the new session opens.
    profile: Option<String>,
    /// The directory the new session's first shell opens in.
    working_directory: Option<PathBuf>,
    /// `Some(true)` starts the session under [`ALLOW_OTHER_USERS_FLAG`].
    is_other_user_access_allowed: Option<bool>,
    /// Where the answer goes.
    response_sender: Sender<RouterResult>,
}

/// A session server this router started that has not reported its socket.
struct StartingSessionServer {
    /// The id the session server was started with.
    session_id: SessionId,
    /// The name the session server was started with.
    session_name: String,
    /// The session server's process.
    child_process: Child,
    /// [`SESSION_SERVER_READY_TIMEOUT_DURATION`] after the start. A session
    /// server whose ready report is not sent by then is killed.
    ready_deadline: Instant,
    /// Set by the thread reading the ready line just before it sends its
    /// report.
    is_ready_report_sent: Arc<AtomicBool>,
    /// Where the answer to its creation goes.
    response_sender: Sender<RouterResult>,
}

impl StartingSessionServer {
    /// The moment this session server is looked at again, while it is still
    /// waited for at `now`: `now` once its ready report is sent, and
    /// [`StartingSessionServer::ready_deadline`] before that. `None` once the
    /// deadline is reached with no report sent.
    fn find_ready_due_at(&self, now: Instant) -> Option<Instant> {
        if self.is_ready_report_sent.load(Ordering::SeqCst) {
            return Some(now);
        }
        (self.ready_deadline > now).then_some(self.ready_deadline)
    }
}

/// Where a description asks a session to describe itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DescribedSessionOrigin {
    /// A session this user started. Its endpoint file in the runtime directory
    /// names its address and its connection token.
    ThisUser,
    /// A session another local user started, at the address the shared
    /// directory names, asked with the empty token.
    OtherUser {
        /// The address the shared directory names.
        socket_address: String,
    },
}

/// What one probe of a session's address found.
#[derive(Debug)]
pub(crate) enum SessionProbeOutcome {
    /// A connection to the address was accepted, and closed again.
    Listening,
    /// Nothing listens at the address.
    NoListener,
    /// The connect failed another way, such as a Windows pipe whose instances
    /// are all busy for [`CONNECT_WAIT_DURATION`](transport::CONNECT_WAIT_DURATION):
    /// something is there.
    Unreachable {
        /// The failure the connect ended in.
        connect_error: IpcError,
    },
    /// The session's endpoint file exists and cannot be read. Nothing was
    /// probed.
    EndpointFileUnreadable {
        /// The failure reading the endpoint file ended in.
        endpoint_file_error: IpcError,
    },
}

/// Which listed session a selector names, as [`resolve_session_selector`]
/// finds it.
#[derive(Debug, PartialEq, Eq)]
enum SessionSelection {
    /// A session of this user's.
    ThisUser(SessionId),
    /// The one session another local user started that the selector names,
    /// while it names no session of this user's.
    OtherUser(SessionId),
    /// A name that several sessions other local users started carry, while no
    /// session of this user's carries it.
    AmbiguousName {
        /// The ids of those sessions, lowest first.
        session_ids: Vec<SessionId>,
    },
    /// No listed session.
    NotListed,
}

/// What applying one probe did to the list, as [`apply_session_probe`] hands
/// it to the lookups waiting on that probe.
#[derive(Debug, PartialEq, Eq)]
enum SessionProbeVerdict {
    /// The session listens. Its record holds the address the probe reached.
    Listening,
    /// Nothing listened at the address probed, and
    /// [`remove_session_from_registry`] did this.
    NothingListening(SessionRemoval),
    /// The connect failed another way, with this sentence.
    Unreachable {
        /// The connect failure, as text.
        connect_error_text: String,
    },
    /// The endpoint file exists and the probe could not read it, with this
    /// sentence.
    EndpointFileUnreadable {
        /// The read failure, as text.
        endpoint_file_error_text: String,
    },
    /// The session was not in the list when the probe's answer arrived.
    NotListed,
}

/// What [`remove_session_from_registry`] did with one session.
#[derive(Debug, PartialEq, Eq)]
enum SessionRemoval {
    /// The session left the list, and every file it left was removed.
    Removed,
    /// Nothing changed: the session is replacing its image.
    ReplacingItsImage,
    /// Nothing changed: the endpoint file no longer holds what the caller
    /// read, or cannot be read. The session bound again since, such as a new
    /// image that wrote a new connection token.
    Rebound,
    /// Nothing changed: [`is_refusal_from_live_session`] accepts the process
    /// its endpoint file names.
    ProcessStillRunning {
        /// The process id the endpoint file names.
        process_id: u32,
    },
}

/// Why a session advertised in the runtime directory or the shared directory
/// is not in the session list: it listens and did not answer, its endpoint
/// file cannot be read, it is replacing its image, its process runs and
/// accepts no connection, it was not asked, or more than one socket advertises
/// its id.
///
/// The text is the reason a lookup refusal names: `NoAnswerInTime` reads
/// `it did not answer within 5 seconds`, `ReplacingItsImage` reads
/// `it is restarting`, and `ProcessStillRunning` for process `5000` reads
/// `process 5000 runs but accepts no connection`.
#[derive(Debug)]
enum UnansweredSessionReason {
    /// The session did not describe itself within
    /// [`SESSION_DISCOVERY_TIMEOUT_DURATION`].
    NoAnswerInTime,
    /// Asking the session failed while something listens at its address, such
    /// as a refused Hello or a protocol version outside this build's range.
    DescriptionFailed {
        /// The failure asking the session ended in.
        description_error: CliError,
    },
    /// The session's endpoint file exists and cannot be read, such as one a
    /// newer build wrote. Nothing of the session is removed.
    EndpointFileUnreadable {
        /// The failure reading the endpoint file ended in.
        endpoint_file_error: IpcError,
    },
    /// Nothing listens at the session's address, and its resume file is
    /// younger than [`RESTART_WINDOW_DURATION`](koshi_ipc::endpoint::RESTART_WINDOW_DURATION).
    ReplacingItsImage,
    /// Nothing accepts a connection at the session's address, and
    /// [`is_refusal_from_live_session`] accepts the process its endpoint file
    /// names.
    ProcessStillRunning {
        /// The process id the endpoint file names.
        process_id: u32,
    },
    /// The session is another local user's, and
    /// [`MAX_OTHER_USER_DESCRIPTIONS_IN_FLIGHT`] descriptions of such sessions
    /// already ran when it was found, so it was not asked.
    TooManyOtherUserDescriptions,
    /// More than one socket in the shared directory advertises the session's
    /// id, so none of them is asked.
    AdvertisedMoreThanOnce {
        /// The id, how many sockets advertise it, and their owners.
        duplicated_session: DuplicatedForeignSession,
    },
}

impl fmt::Display for UnansweredSessionReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnansweredSessionReason::NoAnswerInTime => write!(
                formatter,
                "it did not answer within {} seconds",
                SESSION_DISCOVERY_TIMEOUT_DURATION.as_secs()
            ),
            UnansweredSessionReason::DescriptionFailed { description_error } => {
                write!(formatter, "{description_error}")
            }
            UnansweredSessionReason::EndpointFileUnreadable {
                endpoint_file_error,
            } => write!(formatter, "{endpoint_file_error}"),
            UnansweredSessionReason::ReplacingItsImage => write!(formatter, "it is restarting"),
            UnansweredSessionReason::ProcessStillRunning { process_id } => write!(
                formatter,
                "process {process_id} runs but accepts no connection"
            ),
            UnansweredSessionReason::TooManyOtherUserDescriptions => write!(
                formatter,
                "{MAX_OTHER_USER_DESCRIPTIONS_IN_FLIGHT} sessions other local users started are \
                 already being asked; run the command again"
            ),
            UnansweredSessionReason::AdvertisedMoreThanOnce { duplicated_session } => {
                write!(formatter, "{duplicated_session}")
            }
        }
    }
}

/// One thing for the dispatcher to do.
pub(crate) enum RouterEvent {
    /// A request read off a connection, with the channel its answer goes back
    /// on.
    Request {
        /// What is being asked.
        request_kind: RouterRequestKind,
        /// Where the answer goes.
        response_sender: Sender<RouterResult>,
    },
    /// A session server that is a child of this process has exited.
    ChildExited(SessionId),
    /// A child process has exited and the router has reaped it.
    #[cfg(unix)]
    ChildProcessReaped {
        /// The process id of the child.
        process_id: u32,
    },
    /// A session asked through [`start_session_description`] has answered,
    /// or the asking failed.
    SessionDescribed {
        /// The session asked.
        session_id: SessionId,
        /// Where it was asked.
        described_session_origin: DescribedSessionOrigin,
        /// The endpoint file the description read and connected through, or
        /// `None` when there was none to read, and for another user's session.
        described_endpoint_file: Option<EndpointFile>,
        /// The session's overview, or the failure asking it ended in.
        description_answer: Result<SessionOverview, CliError>,
    },
    /// A probe that [`start_session_probe`] started has connected, or failed
    /// to.
    SessionProbed {
        /// The session probed.
        session_id: SessionId,
        /// The endpoint file the probe read before it connected, or `None`
        /// when there was none, and for another user's session.
        probed_endpoint_file: Option<EndpointFile>,
        /// What the probe found.
        session_probe_outcome: SessionProbeOutcome,
    },
    /// A session server that [`start_session_server`] started has printed its
    /// ready line, or its output ended without one.
    SessionServerReported {
        /// The id the session server was started with.
        session_id: SessionId,
        /// The report it printed, or `None` for nothing readable.
        ready_report: Option<SessionServerReady>,
    },
    /// The router restarts next: a `Restarting` reply has been written to its
    /// connection, or the program file the router started from holds another
    /// koshi version.
    RestartDue,
    /// A question from a remote connection the listener is holding.
    Admission(AdmissionAsk),
}

/// One remote connection this machine has admitted, and the grant that
/// admitted it.
struct AdmittedRemoteConnection {
    /// The sha256 of the secret that admitted this connection.
    token_hash: String,
    /// The connection's socket, held so a revoke can end it.
    tcp_stream: TcpStream,
    /// The number this connection is registered under, which the listener
    /// reports when the connection ends.
    remote_connection_id: u64,
}

/// How many remote connections this machine holds admitted at once.
///
/// Each one keeps a socket handle in [`RemoteState::admitted_remote_connections`] and a thread in
/// the listener. A connection arriving over this count is refused in the sentence every refusal
/// carries, and nothing is registered for it.
pub(crate) const MAX_LIVE_REMOTE_CONNECTION_COUNT: usize = 128;

/// What the router holds for remote clients: where the listener binds, whether
/// it is open, and the connections it has admitted.
///
/// Owned by the dispatcher loop alone, as the session list is.
struct RemoteState {
    /// The IP address and port `koshi.kdl` names, or `None` when it names
    /// none.
    remote_listen_address: Option<SocketAddr>,
    /// The koshi data directory holding the certificate and the record of the
    /// operator's yes, or `None` when this machine has none.
    data_directory: Option<PathBuf>,
    /// Whether the listener is open.
    is_listening: bool,
    /// The remote connections this machine has admitted, whether they have
    /// attached to a session or not. Never longer than [`MAX_LIVE_REMOTE_CONNECTION_COUNT`].
    admitted_remote_connections: Vec<AdmittedRemoteConnection>,
    /// The number the next admitted connection is registered under.
    next_remote_connection_id: u64,
    /// The warning written when the list is full.
    full_capacity_warning: WarningRateLimiter,
}

impl RemoteState {
    /// End every admitted connection a secret in `token_hashes` opened, and drop it
    /// from the list.
    ///
    /// Each connection's socket is shut down in both directions, ending the
    /// thread reading it and its two bridge threads when it has attached. A
    /// new attach on a dropped record is refused.
    ///
    /// Called on a revoke and on a grant that replaces a standing one. An
    /// expiry calls nothing.
    fn close_connections_for_token_hashes(&mut self, token_hashes: &[String]) {
        self.admitted_remote_connections
            .retain(|remote_connection| {
                if !token_hashes.contains(&remote_connection.token_hash) {
                    return true;
                }
                let _ = remote_connection.tcp_stream.shutdown(Shutdown::Both);
                false
            });
    }
}

/// Why the dispatcher loop ended.
#[derive(Debug, PartialEq, Eq)]
enum RouterExit {
    /// No session is running and the idle window passed, or the events
    /// channel closed.
    Idle,
    /// A [`RouterEvent::RestartDue`] arrived and no request waits. The router
    /// restarts into its program file.
    Restart,
    /// A `waitpid` on a child that no thread of this process waits on failed.
    /// [`run_router`] returns the error.
    #[cfg(unix)]
    ChildReaperFailed(ChildReapError),
}

/// A `waitpid` on the child `process_id` failed with an error other than
/// `EINTR` or `ECHILD`.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct ChildReapError {
    /// The process id of the child the `waitpid` named.
    process_id: u32,
    /// The `errno` value the `waitpid` set.
    wait_error_code: i32,
}

#[cfg(unix)]
impl fmt::Display for ChildReapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "child process {} could not be reaped: {}",
            self.process_id,
            std::io::Error::from_raw_os_error(self.wait_error_code)
        )
    }
}

#[cfg(unix)]
impl std::error::Error for ChildReapError {}

/// Run the router until no session is left.
///
/// Takes the advisory lock first: another router already holding it means
/// this call returns `Ok(())` having bound nothing, and the caller connects
/// to that router instead. `should_wait_for_lock` waits up to `LOCK_HANDOVER_TIMEOUT_DURATION`
/// for that router to release it, and yields the same way once the wait runs
/// out. With the lock held, format 1 remote files are converted, the
/// socket is bound, the endpoint file is written, every session already
/// running is asked to describe itself through `create_router_sessions`, and
/// the dispatcher serves requests until an idle window passes with no session
/// running.
///
/// Once the lock is held, the router's program file is written beside its
/// endpoint file, and every connection runs a check of that program file, as
/// `check_router_executable_file` states. A due restart ends the dispatcher
/// and restarts this router into the program file at the path
/// [`resolve_program_path`](koshi_host::program_path::resolve_program_path)
/// gives. A restart that fails writes the program file again, calls
/// [`ExecutableWatch::schedule_restart_retry`], and resumes the dispatcher
/// with everything the router holds untouched. The router removes its program
/// file and its endpoint file when it ends.
///
/// On Unix, once the lock is held and before anything is bound, every child
/// process of this one is listed and adopted: a thread waits on each one and
/// reaps it once it exits. After a restart in place, those are the processes
/// the previous image started. A router started with `should_wait_for_lock`
/// lists none: the router it takes over from started it. A listing that fails
/// starts the next router with `--wait-for-lock`, and this call returns
/// `Ok(())` having bound nothing. The children of this process then pass to
/// init, which reaps each one. If the next router cannot start, this router
/// serves and adopts no child: a child it could not list stays unreaped until
/// this router ends.
///
/// A child process the dispatcher cannot reap ends the router with that error,
/// after the cleanup every other end runs.
///
/// `config_directory` holds the `koshi.kdl` the router reads: at the start for
/// `remote-listen` and the shared directory, on every attach lookup for the
/// shared directory, and on Windows for the marker a removed session leaves.
/// `None` reads no file.
pub fn run_router(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    should_wait_for_lock: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    koshi_paths::ensure_private_directory(runtime_directory)?;

    // The path of the program this router runs, as
    // `koshi_host::program_path::resolve_program_path` reads it at its first
    // call, here. A restart and every session server it starts run the binary
    // at this path. The watch takes the file at that path, as it is now, as the
    // one this router runs.
    let executable_path = koshi_host::program_path::resolve_program_path()?;
    let executable_watch = Arc::new(ExecutableWatch::new(executable_path.clone(), BUILD_VERSION));

    // Where this machine's remote access tokens live, resolved once here. A
    // machine with no resolvable data directory has no store and holds no
    // remote access token.
    let data_directory = koshi_paths::resolve_data_directory();
    let token_store_path = data_directory.as_deref().map(resolve_token_store_path);

    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(resolve_router_lock_path(runtime_directory))?;
    if !take_router_lock(&lock_file, should_wait_for_lock)? {
        return Ok(());
    }
    #[cfg(unix)]
    let inherited_child_process_ids = if should_wait_for_lock {
        Vec::new()
    } else {
        match process::list_child_process_ids() {
            Ok(child_process_ids) => child_process_ids,
            Err(child_process_list_error) => {
                tracing::warn!(
                    %child_process_list_error,
                    "the children of this process could not be listed; the next router takes over"
                );
                match hand_over_router_to_next_process(&executable_path, runtime_directory) {
                    Ok(()) => return Ok(()),
                    Err(handover_error) => {
                        tracing::error!(
                            %handover_error,
                            "the next router could not start; this router serves, and each child it could not list stays unreaped until it ends"
                        );
                        Vec::new()
                    }
                }
            }
        }
    };

    if let Some(data_directory) = data_directory.as_deref() {
        for migration_error in migrate_remote_listener_files(data_directory) {
            tracing::warn!(%migration_error, "remote access files could not be migrated");
        }
    }

    // Trust order: the address is checked against the private directory
    // before anything at it is touched.
    let router_socket_address = compute_router_socket_address(runtime_directory);
    validate_socket_address(&router_socket_address, runtime_directory)?;
    reclaim_stale_socket(&router_socket_address)?;
    let listener = Listener::bind(&router_socket_address)?;

    let router_connection_token = ConnectionToken::generate();
    let endpoint_path = resolve_router_endpoint_path(runtime_directory);
    let router_endpoint_file = EndpointFile {
        socket_address: router_socket_address.clone(),
        connection_token: router_connection_token.clone(),
        process_id: std::process::id(),
    };
    if let Err(endpoint_write_error) = router_endpoint_file.write_to_path(&endpoint_path) {
        drop(listener);
        remove_socket_file(&router_socket_address);
        return Err(endpoint_write_error.into());
    }
    let program_file_path = resolve_router_program_file_path(runtime_directory);
    if let Err(program_file_write_error) = executable_watch
        .build_server_program_file()
        .write_to_path(&program_file_path)
    {
        let _ = std::fs::remove_file(&endpoint_path);
        drop(listener);
        remove_socket_file(&router_socket_address);
        return Err(program_file_write_error.into());
    }

    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let mut router_sessions = create_router_sessions(
        runtime_directory,
        koshi_link::config::find_shared_sessions_base_directory(config_directory).as_deref(),
        &router_events_sender,
    );
    #[cfg(unix)]
    adopt_inherited_children(
        inherited_child_process_ids,
        &mut router_sessions,
        &router_events_sender,
    );

    let is_shutting_down = Arc::new(AtomicBool::new(false));
    let accept_thread = match start_router_accept_thread(
        listener,
        router_connection_token,
        router_events_sender.clone(),
        &is_shutting_down,
        Arc::clone(&executable_watch),
    ) {
        Ok(accept_thread_handle) => accept_thread_handle,
        Err(accept_thread_error) => {
            let _ = std::fs::remove_file(&program_file_path);
            let _ = std::fs::remove_file(&endpoint_path);
            remove_socket_file(&router_socket_address);
            return Err(accept_thread_error.into());
        }
    };

    let mut remote_state = RemoteState {
        remote_listen_address: merge_server(
            ServerConfig::default(),
            koshi_link::config::load_app_layer(config_directory)
                .into_iter()
                .collect(),
        )
        .remote_listen_address,
        data_directory,
        is_listening: false,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };
    open_remote_listener(&mut remote_state, &executable_watch, &router_events_sender);

    let router_end: Result<(), Box<dyn std::error::Error>> = loop {
        match run_dispatch_loop(
            runtime_directory,
            config_directory,
            &executable_watch,
            token_store_path.as_deref(),
            &router_events_sender,
            &router_events_receiver,
            ROUTER_IDLE_TIMEOUT_DURATION,
            SESSION_LIVENESS_CHECK_INTERVAL_DURATION,
            &mut router_sessions,
            &mut remote_state,
        ) {
            RouterExit::Idle => break Ok(()),
            #[cfg(unix)]
            RouterExit::ChildReaperFailed(child_reap_error) => break Err(child_reap_error.into()),
            RouterExit::Restart => {
                #[cfg(unix)]
                {
                    // The program file is removed before the exec: the image
                    // that follows writes its own. The call returns only when
                    // the exec failed, having put the SIGPIPE ignore back; the
                    // loop serves on with the program file written again.
                    let _ = std::fs::remove_file(&program_file_path);
                    let _ = restart_by_exec(&executable_path, runtime_directory);
                    if let Err(program_file_write_error) = executable_watch
                        .build_server_program_file()
                        .write_to_path(&program_file_path)
                    {
                        tracing::error!(
                            %program_file_write_error,
                            "the router program file could not be written again"
                        );
                    }
                }
                #[cfg(windows)]
                // The new router waits for the lock this one drops last; a
                // spawn that failed leaves this router serving.
                if hand_over_router_to_next_process(&executable_path, runtime_directory).is_ok() {
                    break Ok(());
                }
                executable_watch.schedule_restart_retry();
            }
        }
    };

    is_shutting_down.store(true, Ordering::SeqCst);
    // The accept loop sits blocked in `accept`. A bare connection wakes it,
    // and it reads the flag. The connection stays open until the join
    // returns.
    if let Ok(wake_connection) = Connection::connect(&router_socket_address) {
        let _ = accept_thread.join();
        drop(wake_connection);
    }
    let _ = std::fs::remove_file(&program_file_path);
    let _ = std::fs::remove_file(&endpoint_path);
    remove_socket_file(&router_socket_address);
    // Shutdown waits `DRAIN_GRACE_DURATION` and does not join the serving
    // threads. A caller that loses its last reply retries as it does against
    // a router that has exited.
    std::thread::sleep(DRAIN_GRACE_DURATION);

    drop(lock_file);
    router_end
}

/// Take the router lock. `true` means this process holds it, and `false` that
/// another router does.
///
/// Without `should_wait_for_lock` one attempt decides it. With `should_wait_for_lock` the
/// attempt is repeated every [`LOCK_HANDOVER_POLL_INTERVAL_DURATION`] for up to
/// [`LOCK_HANDOVER_TIMEOUT_DURATION`], and a wait that runs out reads as another router
/// holding it.
fn take_router_lock(lock_file: &File, should_wait_for_lock: bool) -> std::io::Result<bool> {
    let lock_wait_deadline = Instant::now() + LOCK_HANDOVER_TIMEOUT_DURATION;
    loop {
        match FileExt::try_lock(lock_file) {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) => {
                if !should_wait_for_lock || Instant::now() >= lock_wait_deadline {
                    return Ok(false);
                }
                std::thread::sleep(LOCK_HANDOVER_POLL_INTERVAL_DURATION);
            }
            Err(TryLockError::Error(lock_error)) => return Err(lock_error),
        }
    }
}

/// Open the remote listener when `koshi.kdl` names an address and the operator
/// has switched remote access on.
///
/// An address alone opens nothing. The port opens the first time the operator
/// answers yes to the offer `koshi share grant` makes, and on every start after
/// that, which is what the record beside the certificate remembers.
///
/// A certificate that cannot be made and an address that cannot be bound are
/// both reported and nothing else changes: local clients are served whatever
/// the remote setting does.
fn open_remote_listener(
    remote_state: &mut RemoteState,
    executable_watch: &Arc<ExecutableWatch>,
    router_events_sender: &Sender<RouterEvent>,
) {
    let Some(remote_listen_address) = remote_state.remote_listen_address else {
        return;
    };
    let Some(data_directory) = remote_state
        .data_directory
        .clone()
        .filter(|data_directory| is_remote_access_enabled(data_directory))
    else {
        tracing::info!(
            "remote listen address {remote_listen_address} is set, but remote access was never switched on; \
             run `koshi share grant` to switch it on"
        );
        return;
    };
    let certificate_file = match load_or_create_certificate(&data_directory) {
        Ok((certificate_file, _)) => certificate_file,
        Err(certificate_error) => {
            tracing::warn!(
                "the remote listener could not open {remote_listen_address}: {certificate_error}; local clients are \
                 unaffected"
            );
            return;
        }
    };
    let bound_listener = match remote_listener::bind_remote_listener(
        remote_listen_address,
        &certificate_file,
    ) {
        Ok(bound_listener) => bound_listener,
        Err(bind_error) => {
            tracing::warn!(
                "the remote listener could not open {remote_listen_address}: {bind_error}; local clients are \
                 unaffected"
            );
            return;
        }
    };
    bound_listener.start_serving(router_events_sender.clone(), Arc::clone(executable_watch));
    remote_state.is_listening = true;
}

/// This machine's certificate and its fingerprint, generating one when there is
/// none to read.
///
/// The certificate koshi generates names `koshi`. A dialling client pins the
/// fingerprint of the certificate it was shown and checks nothing else about
/// it.
///
/// # Errors
/// [`IpcError::RemoteFileUnreadable`] for an existing certificate that cannot
/// be read. [`IpcError::RemoteFileWrite`] names a certificate that could not
/// be generated or written.
fn load_or_create_certificate(
    data_directory: &Path,
) -> Result<(CertificateFile, String), IpcError> {
    let certificate_file_path = CertificateFile::resolve_certificate_file_path(data_directory);
    match std::fs::symlink_metadata(&certificate_file_path) {
        Ok(_) => {
            let certificate_file = CertificateFile::load_from_path(&certificate_file_path)?;
            let certificate_fingerprint =
                tls::compute_certificate_fingerprint(&certificate_file.cert_der);
            return Ok((certificate_file, certificate_fingerprint));
        }
        Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => {}
        Err(read_error) => {
            return Err(IpcError::RemoteFileUnreadable {
                remote_file: RemoteFile::Certificate,
                remote_file_path: certificate_file_path.display().to_string(),
                error_detail: read_error.to_string(),
            });
        }
    }
    let generated_certificate = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .map_err(|certificate_generation_error| IpcError::RemoteFileWrite {
            remote_file: RemoteFile::Certificate,
            remote_file_path: certificate_file_path.display().to_string(),
            error_detail: format!(
                "the certificate could not be generated: {certificate_generation_error}"
            ),
        })?;
    let certificate_file = CertificateFile {
        file_format: CERTIFICATE_FILE_FORMAT,
        cert_der: generated_certificate.cert.der().to_vec(),
        key_der: generated_certificate.signing_key.serialize_der(),
    };
    certificate_file.write_to_path(&certificate_file_path)?;
    let certificate_fingerprint = tls::compute_certificate_fingerprint(&certificate_file.cert_der);
    Ok((certificate_file, certificate_fingerprint))
}

/// Replace this process's running image with the binary at `executable_path`, serving the
/// same runtime directory. The call returns only when the exec failed, and
/// hands back that error, on the terms
/// [`exec_and_keep_ignoring_sigpipe`](koshi_link::process::exec_and_keep_ignoring_sigpipe)
/// states.
///
/// A successful exec closes the router lock file with every other descriptor
/// the standard library opened close-on-exec. The new image's [`run_router`]
/// then takes the lock, reclaims the socket path, binds, writes a fresh
/// endpoint file, and asks every running session to describe itself — under
/// the same process id.
#[cfg(unix)]
fn restart_by_exec(executable_path: &Path, runtime_directory: &Path) -> std::io::Error {
    koshi_link::process::exec_and_keep_ignoring_sigpipe(
        std::process::Command::new(executable_path)
            .arg(ROUTER_SUBCOMMAND)
            .arg(RUNTIME_DIRECTORY_FLAG)
            .arg(runtime_directory),
    )
}

/// Start the binary at `executable_path` as a new router over the same runtime directory,
/// waiting for the lock this router still holds.
///
/// The new router is detached as [`process::configure_detached_process`] sets
/// it. An error means nothing was started.
fn hand_over_router_to_next_process(
    executable_path: &Path,
    runtime_directory: &Path,
) -> std::io::Result<()> {
    process::configure_detached_process(&mut std::process::Command::new(executable_path))
        .arg(ROUTER_SUBCOMMAND)
        .arg(RUNTIME_DIRECTORY_FLAG)
        .arg(runtime_directory)
        .arg(WAIT_FOR_LOCK_FLAG)
        .spawn()
        .map(|_| ())
}

/// Start the thread that accepts router connections.
fn start_router_accept_thread(
    listener: Listener,
    router_connection_token: ConnectionToken,
    router_events_sender: Sender<RouterEvent>,
    is_shutting_down: &Arc<AtomicBool>,
    executable_watch: Arc<ExecutableWatch>,
) -> std::io::Result<JoinHandle<()>> {
    let shutdown_flag = Arc::clone(is_shutting_down);
    std::thread::Builder::new()
        .name("koshi-router-accept".to_string())
        .spawn(move || {
            run_router_accept_loop(
                &listener,
                &router_connection_token,
                &router_events_sender,
                &shutdown_flag,
                &executable_watch,
            )
        })
}

/// Accept connections until the shutdown flag is set, giving each its own
/// serving thread.
///
/// Only this router's own user is served. A connection opened by another user,
/// and one whose user cannot be read, is closed without being served. Each
/// served connection runs a check of `executable_watch`, as
/// [`check_router_executable_file`] states.
fn run_router_accept_loop(
    listener: &Listener,
    router_connection_token: &ConnectionToken,
    router_events_sender: &Sender<RouterEvent>,
    is_shutting_down: &AtomicBool,
    executable_watch: &Arc<ExecutableWatch>,
) {
    transport::accept_until_shutdown(
        listener,
        is_shutting_down,
        ACCEPT_RETRY_DELAY_DURATION,
        |connection| {
            // The OS reports which user opened the connection.
            if !matches!(connection.is_peer_same_user(), Ok(true)) {
                return;
            }
            check_router_executable_file(executable_watch, router_events_sender);
            let router_connection_token = router_connection_token.clone();
            let router_events_sender = router_events_sender.clone();
            let executable_watch = Arc::clone(executable_watch);
            std::thread::spawn(move || {
                serve_router_connection(
                    connection,
                    router_connection_token,
                    &router_events_sender,
                    &executable_watch,
                )
            });
        },
    );
}

/// Run a check of `executable_watch`. When the router's program file holds
/// another koshi version, [`RouterEvent::RestartDue`] goes to the dispatcher
/// on `router_events_sender`, and the version is logged at info level.
pub(crate) fn check_router_executable_file(
    executable_watch: &Arc<ExecutableWatch>,
    router_events_sender: &Sender<RouterEvent>,
) {
    executable_watch.check_executable_file(build_router_restart_trigger(router_events_sender));
}

/// What a check of the router's program file runs once the file prints another
/// koshi version: log that version at info level and send
/// [`RouterEvent::RestartDue`] on `router_events_sender`.
fn build_router_restart_trigger(
    router_events_sender: &Sender<RouterEvent>,
) -> impl FnOnce(String) + Send + 'static {
    let restart_events_sender = router_events_sender.clone();
    move |installed_version| {
        tracing::info!(
            installed_version,
            "the program file holds another koshi version; the router restarts into it"
        );
        // A send that fails means the dispatcher is gone and the router is
        // already exiting.
        let _ = restart_events_sender.send(RouterEvent::RestartDue);
    }
}

/// Serve one router connection until its peer hangs up or a fault closes it.
///
/// [`plane::read_next_request`] makes every decision that is the same on every
/// koshi protocol — the framing faults, a request kind this build does not
/// have, and the Hello. What is left crosses to the dispatcher and comes back
/// as its answer.
///
/// A `Restarting` answer that has been written is reported to the dispatcher,
/// and this connection keeps serving until the router ends. A Hello refused for
/// its protocol version runs
/// [`ExecutableWatch::check_executable_file_after_refused_hello`] on
/// `executable_watch`, which sends [`RouterEvent::RestartDue`] when the program
/// file holds another koshi version.
///
/// On Unix the thread blocks SIGPIPE on its own signal mask first; a write to
/// a peer that hung up returns an error whatever the process-wide disposition
/// is.
fn serve_router_connection(
    mut connection: Connection,
    router_connection_token: ConnectionToken,
    router_events_sender: &Sender<RouterEvent>,
    executable_watch: &Arc<ExecutableWatch>,
) {
    #[cfg(unix)]
    process::block_sigpipe_on_this_thread();
    let mut router_handshake = RouterHandshake::from_connection_token(router_connection_token);
    loop {
        let (request_id, request_kind) = match plane::read_next_request::<ControlPlane>(
            &mut connection,
            &mut router_handshake,
            BUILD_VERSION,
            &|| true,
        ) {
            RequestDisposition::Answered => continue,
            RequestDisposition::VersionRefused => {
                executable_watch.check_executable_file_after_refused_hello(
                    build_router_restart_trigger(router_events_sender),
                );
                continue;
            }
            RequestDisposition::Stop => return,
            RequestDisposition::Dispatch {
                request_id,
                request_kind,
            } => (request_id, request_kind),
        };

        let Some(answer_result) = ask_dispatcher(router_events_sender, request_kind) else {
            return;
        };
        let router_response = RouterResponse {
            request_id: Some(request_id),
            answer_result,
        };
        if connection.send(&router_response).is_err() {
            return;
        }
        if router_response.answer_result == RouterResult::Restarting {
            // A send that fails means the dispatcher is gone and the router is
            // already exiting.
            let _ = router_events_sender.send(RouterEvent::RestartDue);
        }
    }
}

/// Hand one request to the dispatcher and wait for its answer. `None` means
/// the dispatcher is gone and the router is exiting: the caller closes its
/// connection without an answer.
fn ask_dispatcher(
    router_events_sender: &Sender<RouterEvent>,
    request_kind: RouterRequestKind,
) -> Option<RouterResult> {
    let (response_sender, response_receiver) = mpsc::channel();
    if router_events_sender
        .send(RouterEvent::Request {
            request_kind,
            response_sender,
        })
        .is_err()
    {
        return None;
    }
    response_receiver.recv().ok()
}

/// Serve events until the router ends, leaving the session list as it stood.
///
/// Each turn of the loop first runs a liveness round once
/// `liveness_check_interval` has passed since the last one: on Unix it reaps
/// every exited child in [`RouterSessions::unwaited_child_process_ids`]
/// through [`reap_unwaited_children`], and then it starts a probe of every
/// session no thread watches. It then answers every waiting request that can
/// be answered, and waits for the next event:
///
/// - While no session is listed and no request waits, it waits
///   `idle_timeout`. A window that passes ends the loop with
///   [`RouterExit::Idle`]. A child process holds no session and keeps no loop
///   running.
/// - Otherwise it waits until the next liveness round, the next moment a
///   waiting request stops waiting for a description, or the next deadline,
///   whichever comes first. The next liveness round counts while a session no
///   thread watches is listed, or while a child waits in
///   [`RouterSessions::unwaited_child_process_ids`]. With none of those due, it
///   waits for the next event.
///
/// A reap that fails ends the loop with [`RouterExit::ChildReaperFailed`].
///
/// Each event goes through [`serve_router_event`]. Once
/// [`RouterSessions::is_restart_pending`] holds and no request waits, the loop
/// ends with [`RouterExit::Restart`]: the caller restarts this router into the
/// program file `executable_watch` checks.
///
/// `router_events_sender` is the loop's own sender, handed to every thread
/// that reports back. `token_store_path` is the file of the remote access
/// token store every token request is answered against, and `remote_state` is
/// what the router holds for remote clients.
///
/// `config_directory` is the one [`run_router`] was given.
#[allow(clippy::too_many_arguments)]
fn run_dispatch_loop(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    executable_watch: &Arc<ExecutableWatch>,
    token_store_path: Option<&Path>,
    router_events_sender: &Sender<RouterEvent>,
    router_events_receiver: &Receiver<RouterEvent>,
    idle_timeout: Duration,
    liveness_check_interval: Duration,
    router_sessions: &mut RouterSessions,
    remote_state: &mut RemoteState,
) -> RouterExit {
    let mut next_liveness_check_at = Instant::now() + liveness_check_interval;
    loop {
        if Instant::now() >= next_liveness_check_at {
            #[cfg(unix)]
            if let Err(child_reap_error) =
                reap_unwaited_children(&mut router_sessions.unwaited_child_process_ids)
            {
                tracing::error!(%child_reap_error, "the router ends");
                return RouterExit::ChildReaperFailed(child_reap_error);
            }
            start_unwatched_session_probes(
                runtime_directory,
                router_sessions,
                router_events_sender,
            );
            next_liveness_check_at = Instant::now() + liveness_check_interval;
        }
        let turn_started_at = Instant::now();
        advance_waiting_requests(
            runtime_directory,
            config_directory,
            router_sessions,
            remote_state,
            router_events_sender,
            turn_started_at,
        );
        let has_waiting_request = router_sessions.has_waiting_request();
        if router_sessions.is_restart_pending && !has_waiting_request {
            router_sessions.is_restart_pending = false;
            return RouterExit::Restart;
        }
        let has_unwatched_session = router_sessions
            .session_registry
            .values()
            .any(|session_record| !session_record.has_exit_watcher);
        #[cfg(unix)]
        let has_unwaited_child = !router_sessions.unwaited_child_process_ids.is_empty();
        #[cfg(not(unix))]
        let has_unwaited_child = false;
        let next_wake_at = [
            (has_unwatched_session || has_unwaited_child).then_some(next_liveness_check_at),
            find_next_waiting_request_wake_at(router_sessions, turn_started_at),
        ]
        .into_iter()
        .flatten()
        .min();
        let received_event = if router_sessions.session_registry.is_empty() && !has_waiting_request
        {
            router_events_receiver.recv_timeout(idle_timeout).ok()
        } else if let Some(next_wake_at) = next_wake_at {
            match router_events_receiver
                .recv_timeout(next_wake_at.saturating_duration_since(Instant::now()))
            {
                Err(RecvTimeoutError::Timeout) => continue,
                receive_outcome => receive_outcome.ok(),
            }
        } else {
            router_events_receiver.recv().ok()
        };
        let Some(router_event) = received_event else {
            return RouterExit::Idle;
        };
        serve_router_event(
            runtime_directory,
            config_directory,
            executable_watch,
            token_store_path,
            router_sessions,
            remote_state,
            router_events_sender,
            router_event,
        );
    }
}

/// Serve one event.
///
/// A [`RouterEvent::ChildExited`] for a listed session sets its
/// `has_exit_watcher` to `false` and starts a probe of it. A report for a
/// session the list does not hold changes nothing. A
/// [`RouterEvent::RestartDue`] sets [`RouterSessions::is_restart_pending`]:
/// [`run_dispatch_loop`] gives [`RouterExit::Restart`] on its first turn with
/// no request waiting.
///
/// `config_directory` is the one [`run_router`] was given.
#[allow(clippy::too_many_arguments)]
fn serve_router_event(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    executable_watch: &Arc<ExecutableWatch>,
    token_store_path: Option<&Path>,
    router_sessions: &mut RouterSessions,
    remote_state: &mut RemoteState,
    router_events_sender: &Sender<RouterEvent>,
    router_event: RouterEvent,
) {
    match router_event {
        RouterEvent::Request {
            request_kind,
            response_sender,
        } => serve_router_request(
            runtime_directory,
            config_directory,
            executable_watch,
            token_store_path,
            router_sessions,
            remote_state,
            router_events_sender,
            request_kind,
            response_sender,
        ),
        RouterEvent::ChildExited(session_id) => {
            if let Some(session_record) = router_sessions.session_registry.get_mut(&session_id) {
                session_record.has_exit_watcher = false;
                let _ = start_session_probe(
                    runtime_directory,
                    router_sessions,
                    session_id,
                    router_events_sender,
                );
            }
        }
        #[cfg(unix)]
        RouterEvent::ChildProcessReaped { process_id } => {
            router_sessions
                .inherited_child_process_ids
                .remove(&process_id);
            let exited_session_ids: Vec<SessionId> = router_sessions
                .session_registry
                .iter()
                .filter(|(_, session_record)| session_record.process_id == process_id)
                .map(|(session_id, _)| *session_id)
                .collect();
            for exited_session_id in exited_session_ids {
                if let Some(session_record) =
                    router_sessions.session_registry.get_mut(&exited_session_id)
                {
                    session_record.has_exit_watcher = false;
                }
                let _ = start_session_probe(
                    runtime_directory,
                    router_sessions,
                    exited_session_id,
                    router_events_sender,
                );
            }
        }
        RouterEvent::SessionDescribed {
            session_id,
            described_session_origin,
            described_endpoint_file,
            description_answer,
        } => apply_session_description(
            runtime_directory,
            config_directory,
            router_sessions,
            router_events_sender,
            session_id,
            &described_session_origin,
            described_endpoint_file.as_ref(),
            description_answer,
        ),
        RouterEvent::SessionProbed {
            session_id,
            probed_endpoint_file,
            session_probe_outcome,
        } => {
            let session_probe_verdict = apply_session_probe(
                runtime_directory,
                config_directory,
                router_sessions,
                session_id,
                probed_endpoint_file.as_ref(),
                session_probe_outcome,
            );
            hand_probe_verdict_to_attach_lookups(
                runtime_directory,
                router_sessions,
                router_events_sender,
                session_id,
                &session_probe_verdict,
            );
        }
        RouterEvent::SessionServerReported {
            session_id,
            ready_report,
        } => finish_session_creation(
            runtime_directory,
            config_directory,
            router_sessions,
            router_events_sender,
            session_id,
            ready_report,
        ),
        RouterEvent::RestartDue => router_sessions.is_restart_pending = true,
        RouterEvent::Admission(admission_question) => {
            serve_remote_admission(
                runtime_directory,
                token_store_path,
                router_sessions,
                remote_state,
                router_events_sender,
                admission_question,
            );
        }
    }
}

/// Answer one request against the session list, now or once what it waits
/// for arrives.
///
/// An attach lookup goes through [`start_attach_lookup`], and a session
/// creation joins [`RouterSessions::queued_session_creations`]. While
/// [`RouterSessions::is_restart_pending`] holds, both are refused with
/// [`ROUTER_RESTARTING_MESSAGE`] instead. Every other request is
/// answered on `response_sender` before this returns. The dispatcher answers
/// one request at a time: the remote access token store at `token_store_path`
/// has one writer.
///
/// `config_directory` is the one [`run_router`] was given. An attach lookup
/// reads its `koshi.kdl` again for the shared directory.
#[allow(clippy::too_many_arguments)]
fn serve_router_request(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    executable_watch: &Arc<ExecutableWatch>,
    token_store_path: Option<&Path>,
    router_sessions: &mut RouterSessions,
    remote_state: &mut RemoteState,
    router_events_sender: &Sender<RouterEvent>,
    request_kind: RouterRequestKind,
    response_sender: Sender<RouterResult>,
) {
    let router_result = match request_kind {
        RouterRequestKind::Hello { .. } => {
            unreachable!("Hello is answered by the connection thread before dispatch")
        }
        RouterRequestKind::CreateSession { .. } | RouterRequestKind::AttachLookup { .. }
            if router_sessions.is_restart_pending =>
        {
            build_refused_result(ROUTER_RESTARTING_MESSAGE.to_string())
        }
        RouterRequestKind::CreateSession {
            profile,
            working_directory,
            is_other_user_access_allowed,
        } => {
            router_sessions
                .queued_session_creations
                .push(QueuedSessionCreation {
                    profile,
                    working_directory,
                    is_other_user_access_allowed,
                    response_sender,
                });
            return;
        }
        RouterRequestKind::AttachLookup { session_selector } => {
            match list_foreign_sessions_for_lookup(
                koshi_link::config::find_shared_sessions_base_directory(config_directory)
                    .as_deref(),
                runtime_directory,
                &router_sessions.session_registry,
                &session_selector,
            ) {
                Ok(foreign_session_listing) => start_attach_lookup(
                    runtime_directory,
                    router_sessions,
                    router_events_sender,
                    foreign_session_listing,
                    session_selector,
                    response_sender,
                ),
                Err(foreign_session_lookup_error) => {
                    let _ = response_sender.send(build_refused_result(
                        foreign_session_lookup_error.to_string(),
                    ));
                }
            }
            return;
        }
        RouterRequestKind::Restart => check_restart_binary(executable_watch.get_executable_path()),
        RouterRequestKind::GrantToken {
            identity,
            scope,
            expires_in,
        } => grant_token(token_store_path, remote_state, identity, scope, expires_in),
        RouterRequestKind::RevokeToken { identity, scope } => {
            revoke_token(token_store_path, remote_state, &identity, scope.as_ref())
        }
        RouterRequestKind::ListTokens { scope } => {
            list_token_entries(token_store_path, scope.as_ref())
        }
        RouterRequestKind::RemoteStatus => build_remote_status_result(remote_state),
        RouterRequestKind::EnableRemote => {
            enable_remote_access(remote_state, executable_watch, router_events_sender)
        }
    };
    let _ = response_sender.send(router_result);
}

/// Answer one question from a remote connection the listener is holding.
///
/// A question for the session rows or for one session's address first asks
/// every session of this user's the list does not hold to describe itself,
/// through [`start_unlisted_session_descriptions`]. Sessions other local users
/// started are not asked. The rows are answered at once, from the list as it
/// stands. A question for one session's address
/// joins [`RouterSessions::waiting_remote_locates`] and is answered once the
/// descriptions it started have answered, or
/// [`SESSION_DISCOVERY_TIMEOUT_DURATION`] after it arrived.
///
/// The dispatcher answers one at a time: the token store has one writer, and
/// the list of admitted connections has one owner.
fn serve_remote_admission(
    runtime_directory: &Path,
    token_store_path: Option<&Path>,
    router_sessions: &mut RouterSessions,
    remote_state: &mut RemoteState,
    router_events_sender: &Sender<RouterEvent>,
    admission_question: AdmissionAsk,
) {
    match admission_question {
        AdmissionAsk::Admit {
            connection_token,
            remote_connection_stream,
            response_sender,
        } => {
            let _ = response_sender.send(admit_remote_token(
                token_store_path,
                remote_state,
                &connection_token,
                remote_connection_stream,
            ));
        }
        AdmissionAsk::ListRows {
            scope,
            response_sender,
        } => {
            start_unlisted_session_descriptions(
                runtime_directory,
                router_sessions,
                router_events_sender,
                list_own_sessions_or_none(runtime_directory),
                ForeignSessionListing::default(),
            );
            let _ = response_sender.send(list_remote_session_rows(
                &router_sessions.session_registry,
                &scope,
            ));
        }
        AdmissionAsk::Locate {
            scope,
            remote_connection_id,
            session_selector,
            response_sender,
        } => {
            if router_sessions.is_restart_pending {
                let _ = response_sender.send(Err(LocateRefusal::RouterRestarting));
                return;
            }
            let awaited_session_ids = start_unlisted_session_descriptions(
                runtime_directory,
                router_sessions,
                router_events_sender,
                list_own_sessions_or_none(runtime_directory),
                ForeignSessionListing::default(),
            );
            router_sessions
                .waiting_remote_locates
                .push(WaitingRemoteLocate {
                    scope,
                    remote_connection_id,
                    session_selector,
                    awaited_session_ids,
                    answer_deadline: Instant::now() + SESSION_DISCOVERY_TIMEOUT_DURATION,
                    response_sender,
                });
        }
        AdmissionAsk::RemoveConnection {
            remote_connection_id,
        } => remote_state
            .admitted_remote_connections
            .retain(|remote_connection| {
                remote_connection.remote_connection_id != remote_connection_id
            }),
    }
}

/// What a presented secret reaches, and the number the connection presenting
/// it is registered under.
///
/// A secret that reaches something registers the connection against that
/// secret's hash, whether or not it goes on to attach. The registration is
/// dropped when the listener reports the connection ended.
///
/// A list already holding [`MAX_LIVE_REMOTE_CONNECTION_COUNT`] admits nothing
/// more. That count is read before the secret.
///
/// The store is written back, stamping that record's last-used time. A store
/// that cannot be read or written admits nothing.
fn admit_remote_token(
    token_store_path: Option<&Path>,
    remote_state: &mut RemoteState,
    connection_token: &ConnectionToken,
    remote_connection_stream: TcpStream,
) -> Option<RemoteConnectionAdmission> {
    if remote_state.admitted_remote_connections.len() >= MAX_LIVE_REMOTE_CONNECTION_COUNT {
        if remote_state.full_capacity_warning.is_due(Instant::now()) {
            tracing::warn!(
                "{MAX_LIVE_REMOTE_CONNECTION_COUNT} remote connections are already admitted; \
                 refusing the ones that arrive until some of them end"
            );
        }
        return None;
    }
    let Ok((token_store_path, mut token_store)) = open_token_store(token_store_path) else {
        return None;
    };
    let scope = token_store.admit_token_scope(connection_token, SystemTime::now())?;
    token_store
        .write_token_store_to_path(token_store_path)
        .ok()?;
    let remote_connection_id = remote_state.next_remote_connection_id;
    remote_state.next_remote_connection_id += 1;
    remote_state
        .admitted_remote_connections
        .push(AdmittedRemoteConnection {
            token_hash: hash_connection_token(connection_token),
            tcp_stream: remote_connection_stream,
            remote_connection_id,
        });
    Some(RemoteConnectionAdmission {
        scope,
        remote_connection_id,
    })
}

/// Whether the session `session_record` describes is one this user started.
///
/// A session of this user's carries the process id of its session server,
/// which is never `0`. A session another local user started carries
/// `process_id` `0`.
///
/// [`list_remote_session_rows`] and [`resolve_session_selector`] both read
/// this: a remote caller is shown and carried to this user's sessions only.
fn is_session_of_this_user(session_record: &SessionRecord) -> bool {
    session_record.process_id != 0
}

/// The sessions an admitted scope reaches, in name then id order.
///
/// A host-wide scope reaches every session of this user's; a session scope
/// reaches that one session. A session another local user started is left out,
/// on the rule [`is_session_of_this_user`] states. Nothing outside the
/// router's own list is read.
fn list_remote_session_rows(
    session_registry: &SessionRegistry,
    scope: &TokenScope,
) -> Vec<RemoteSessionRow> {
    let mut remote_session_rows: Vec<RemoteSessionRow> = session_registry
        .iter()
        .filter(|(session_id, session_record)| {
            scope.is_allowed_for_session(**session_id) && is_session_of_this_user(session_record)
        })
        .map(|(session_id, session_record)| RemoteSessionRow {
            session_id: *session_id,
            session_name: session_record.session_name.clone(),
        })
        .collect();
    remote_session_rows.sort_by(|left_row, right_row| {
        left_row
            .session_name
            .cmp(&right_row.session_name)
            .then(left_row.session_id.cmp(&right_row.session_id))
    });
    remote_session_rows
}

/// The endpoint file of the session an admitted client asked for, when the
/// connection numbered `remote_connection_id` still stands, the session is one
/// of this user's, and its scope covers that session.
///
/// Checks in this order: the connection numbered `remote_connection_id` is
/// still registered, [`resolve_session_selector`] names a session of this
/// user's in the router's own in-memory list, and `scope` covers that session.
/// No socket is opened, nothing is waited for, and no file is touched.
///
/// `None` for all three failures: a connection a revoke dropped, a session
/// selector naming no session of this user's, and a session the scope does not
/// cover.
fn locate_remote_session(
    runtime_directory: &Path,
    session_registry: &SessionRegistry,
    remote_state: &RemoteState,
    scope: &TokenScope,
    remote_connection_id: u64,
    session_selector: &SessionSelector,
) -> Option<PathBuf> {
    if !remote_state
        .admitted_remote_connections
        .iter()
        .any(|remote_connection| remote_connection.remote_connection_id == remote_connection_id)
    {
        return None;
    }
    let SessionSelection::ThisUser(session_id) =
        resolve_session_selector(session_registry, session_selector)
    else {
        return None;
    };
    if !scope.is_allowed_for_session(session_id) {
        return None;
    }
    Some(EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
}

/// What this machine's remote access is set to: the address `koshi.kdl` names,
/// whether the operator has switched remote access on, whether this router is
/// holding the port right now, the fingerprint of the certificate this machine
/// presents once it has one, and how many connections from another machine
/// this router holds admitted.
///
/// `is_remote_access_enabled` and `is_listening` are separate answers: an
/// operator who said yes on a machine whose address something else holds reads
/// `is_remote_access_enabled: true` and `is_listening: false`.
fn build_remote_status_result(remote_state: &RemoteState) -> RouterResult {
    let data_directory = remote_state.data_directory.as_deref();
    RouterResult::RemoteStatus {
        remote_listen_address: remote_state.remote_listen_address,
        is_remote_access_enabled: data_directory.is_some_and(is_remote_access_enabled),
        is_listening: remote_state.is_listening,
        certificate_fingerprint: data_directory
            .and_then(|data_directory| {
                CertificateFile::load_from_path(&CertificateFile::resolve_certificate_file_path(
                    data_directory,
                ))
                .ok()
            })
            .map(|certificate_file| {
                tls::compute_certificate_fingerprint(&certificate_file.cert_der)
            }),
        remote_connection_count: remote_state.admitted_remote_connections.len(),
    }
}

/// Switch remote access on, in four steps: make this machine's certificate when
/// it has none, take the port when it is not already held, write the record that
/// reopens it on the next start, then serve.
///
/// A port that cannot be taken writes no record. A record that cannot be written
/// gives the port back. Serving is last and cannot fail.
///
/// An address already being listened on skips the bind and the serve, and is
/// answered with the fingerprint it presents.
fn enable_remote_access(
    remote_state: &mut RemoteState,
    executable_watch: &Arc<ExecutableWatch>,
    router_events_sender: &Sender<RouterEvent>,
) -> RouterResult {
    let Some(remote_listen_address) = remote_state.remote_listen_address else {
        return build_refused_result(
            "no remote listen address is set; add `remote-listen \"<ip>:<port>\"` to koshi.kdl"
                .to_string(),
        );
    };
    let Some(data_directory) = remote_state.data_directory.clone() else {
        return build_refused_result(
            "this machine has no data directory, so remote access cannot be switched on"
                .to_string(),
        );
    };
    let (certificate_file, certificate_fingerprint) =
        match load_or_create_certificate(&data_directory) {
            Ok(loaded_certificate) => loaded_certificate,
            Err(certificate_error) => return build_refused_result(certificate_error.to_string()),
        };

    let bound_listener = if remote_state.is_listening {
        None
    } else {
        match remote_listener::bind_remote_listener(remote_listen_address, &certificate_file) {
            Ok(bound_listener) => Some(bound_listener),
            Err(bind_error) => {
                return build_refused_result(format!(
                    "the remote listener could not open {remote_listen_address}: {bind_error}"
                ))
            }
        }
    };

    let remote_access_record = RemoteAccessRecord {
        file_format: REMOTE_ACCESS_RECORD_FILE_FORMAT,
        enabled_at: SystemTime::now(),
    };
    if let Err(remote_access_record_write_error) = remote_access_record.write_to_path(
        &RemoteAccessRecord::resolve_remote_access_record_path(&data_directory),
    ) {
        // Dropping the bound port gives it back.
        drop(bound_listener);
        return build_refused_result(remote_access_record_write_error.to_string());
    }

    if let Some(bound_listener) = bound_listener {
        bound_listener.start_serving(router_events_sender.clone(), Arc::clone(executable_watch));
        remote_state.is_listening = true;
    }
    RouterResult::RemoteEnabled {
        remote_listen_address,
        certificate_fingerprint,
    }
}

/// The remote access token store at `token_store_path`, with the path to write
/// it back to.
///
/// `None` means this machine has no data directory to hold a store. A store
/// whose bytes cannot be read is refused: every token request is refused, and
/// nothing changes.
fn open_token_store(token_store_path: Option<&Path>) -> Result<(&Path, TokenStore), RouterResult> {
    let Some(token_store_path) = token_store_path else {
        return Err(build_refused_result(
            "this machine has no data directory, so no remote access token can be stored"
                .to_string(),
        ));
    };
    match TokenStore::load_token_store_from_path(token_store_path) {
        Ok(token_store) => Ok((token_store_path, token_store)),
        Err(token_store_read_error) => {
            Err(build_refused_result(token_store_read_error.to_string()))
        }
    }
}

/// Hand `identity` a fresh secret on `scope` and write the store back.
///
/// The clock is read once, and both the issue time and the expiry are stamped
/// from that one reading. `expires_in` is added to the issue time with a
/// checked add. A span the clock cannot represent is refused before anything
/// is written: the store file is left as it stood.
///
/// A grant takes the place of whatever `identity` held on `scope`. Every
/// connection the replaced secret admitted ends once the new record is
/// written. The replaced hashes are read before the replace.
fn grant_token(
    token_store_path: Option<&Path>,
    remote_state: &mut RemoteState,
    identity: String,
    scope: TokenScope,
    expires_in: Option<Duration>,
) -> RouterResult {
    let (token_store_path, mut token_store) = match open_token_store(token_store_path) {
        Ok(opened_token_store) => opened_token_store,
        Err(refusal_result) => return refusal_result,
    };
    let issued_at = SystemTime::now();
    let expires_at = match expires_in {
        None => None,
        Some(expiry_duration) => match issued_at.checked_add(expiry_duration) {
            Some(expiry_time) => Some(expiry_time),
            None => {
                return build_refused_result(
                    "the expiry is further ahead than this machine's clock can represent"
                        .to_string(),
                )
            }
        },
    };
    let replaced_token_hashes: Vec<String> = token_store
        .token_records
        .iter()
        .filter(|token_record| {
            token_record.identity == identity
                && token_record.scope == scope
                && token_record.revoked_at.is_none()
                && token_record
                    .expires_at
                    .is_none_or(|expiry_time| expiry_time > issued_at)
        })
        .map(|token_record| token_record.token_hash.clone())
        .collect();
    let (connection_token, has_replaced_active_grant) =
        token_store.grant_token(identity, scope, issued_at, expires_at);
    if let Err(token_store_write_error) = token_store.write_token_store_to_path(token_store_path) {
        return build_refused_result(token_store_write_error.to_string());
    }
    remote_state.close_connections_for_token_hashes(&replaced_token_hashes);
    RouterResult::Granted {
        connection_token,
        has_replaced_active_grant,
    }
}

/// Stop the grants `identity` holds, narrowed to one scope when `scope` is
/// given, and write the store back when this call stopped anything.
///
/// Every connection those grants admitted ends once the store is written. A
/// connection that never attached ends with the rest. The revoked hashes are
/// read before the revoke.
fn revoke_token(
    token_store_path: Option<&Path>,
    remote_state: &mut RemoteState,
    identity: &str,
    scope: Option<&TokenScope>,
) -> RouterResult {
    let (token_store_path, mut token_store) = match open_token_store(token_store_path) {
        Ok(opened_token_store) => opened_token_store,
        Err(refusal_result) => return refusal_result,
    };
    let revoked_token_hashes: Vec<String> = token_store
        .token_records
        .iter()
        .filter(|token_record| {
            token_record.identity == identity
                && token_record.revoked_at.is_none()
                && scope.is_none_or(|requested_scope| *requested_scope == token_record.scope)
        })
        .map(|token_record| token_record.token_hash.clone())
        .collect();
    let revoked_scopes = token_store.revoke_token_grants(identity, scope, SystemTime::now());
    if revoked_scopes.is_empty() {
        return RouterResult::Revoked(revoked_scopes);
    }
    if let Err(token_store_write_error) = token_store.write_token_store_to_path(token_store_path) {
        return build_refused_result(token_store_write_error.to_string());
    }
    remote_state.close_connections_for_token_hashes(&revoked_token_hashes);
    RouterResult::Revoked(revoked_scopes)
}

/// Every grant this machine has made, narrowed to the grants that reach
/// `scope` when one is given. The store is not written.
fn list_token_entries(token_store_path: Option<&Path>, scope: Option<&TokenScope>) -> RouterResult {
    match open_token_store(token_store_path) {
        Ok((_, token_store)) => RouterResult::Tokens(token_store.list_token_entries(scope)),
        Err(refusal_result) => refusal_result,
    }
}

/// Answer a restart request by checking the binary at `executable_path`. A binary that
/// cannot be read is refused; on Unix, one with no execute permission is
/// refused too. Nothing is torn down either way.
fn check_restart_binary(executable_path: &Path) -> RouterResult {
    match is_binary_runnable(executable_path) {
        Ok(()) => RouterResult::Restarting,
        Err(binary_check_error) => build_refused_result(binary_check_error),
    }
}

/// Move every waiting request on at `now`, answering each one that waits for
/// nothing more: attach lookups through [`advance_attach_lookup`], remote
/// attaches through [`advance_remote_locate`], and session creations through
/// [`advance_session_creations`].
///
/// `config_directory` is the one [`run_router`] was given.
fn advance_waiting_requests(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    router_sessions: &mut RouterSessions,
    remote_state: &RemoteState,
    router_events_sender: &Sender<RouterEvent>,
    now: Instant,
) {
    for attach_lookup in std::mem::take(&mut router_sessions.waiting_attach_lookups) {
        if let Some(waiting_attach_lookup) = advance_attach_lookup(
            runtime_directory,
            router_sessions,
            router_events_sender,
            attach_lookup,
            now,
        ) {
            router_sessions
                .waiting_attach_lookups
                .push(waiting_attach_lookup);
        }
    }
    for remote_locate in std::mem::take(&mut router_sessions.waiting_remote_locates) {
        if let Some(waiting_remote_locate) = advance_remote_locate(
            runtime_directory,
            router_sessions,
            remote_state,
            remote_locate,
            now,
        ) {
            router_sessions
                .waiting_remote_locates
                .push(waiting_remote_locate);
        }
    }
    advance_session_creations(
        runtime_directory,
        config_directory,
        router_sessions,
        router_events_sender,
        now,
    );
}

/// The next moment a waiting request must be looked at again, at or after
/// `now`: the earliest end of a description wait from
/// [`find_description_wait_end`], and the earliest moment
/// [`StartingSessionServer::find_ready_due_at`] gives. While a creation is
/// queued, the moment [`find_earliest_description_due_at`] gives counts too.
/// `None` when nothing waits on time: a lookup waiting for a probe is answered
/// when the probe reports.
fn find_next_waiting_request_wake_at(
    router_sessions: &RouterSessions,
    now: Instant,
) -> Option<Instant> {
    let attach_lookup_wake_ats = router_sessions
        .waiting_attach_lookups
        .iter()
        .filter(|attach_lookup| attach_lookup.probed_session_id.is_none())
        .filter_map(|attach_lookup| {
            find_description_wait_end(
                router_sessions,
                &attach_lookup.awaited_session_ids,
                attach_lookup.answer_deadline,
                now,
            )
        });
    let remote_locate_wake_ats =
        router_sessions
            .waiting_remote_locates
            .iter()
            .filter_map(|remote_locate| {
                find_description_wait_end(
                    router_sessions,
                    &remote_locate.awaited_session_ids,
                    remote_locate.answer_deadline,
                    now,
                )
            });
    let ready_due_ats = router_sessions
        .starting_session_servers
        .iter()
        .filter_map(|starting_session_server| starting_session_server.find_ready_due_at(now));
    let queued_creation_wake_at = if router_sessions.queued_session_creations.is_empty() {
        None
    } else {
        find_earliest_description_due_at(router_sessions, now)
    };
    attach_lookup_wake_ats
        .chain(remote_locate_wake_ats)
        .chain(ready_due_ats)
        .chain(queued_creation_wake_at)
        .min()
}

/// Move every session creation on at `now`.
///
/// A starting session server that [`StartingSessionServer::find_ready_due_at`]
/// gives `None` for is killed, the files it advertised are removed, and its
/// creation is refused with `the session did not report a bound socket`.
///
/// While [`find_earliest_description_due_at`] gives a moment, every queued
/// creation keeps waiting.
/// Otherwise each queued creation, in arrival order, starts its session server
/// through [`start_session_server`].
///
/// `config_directory` goes to [`refuse_starting_session_server`].
fn advance_session_creations(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    now: Instant,
) {
    for starting_session_server in std::mem::take(&mut router_sessions.starting_session_servers) {
        if starting_session_server.find_ready_due_at(now).is_some() {
            router_sessions
                .starting_session_servers
                .push(starting_session_server);
            continue;
        }
        refuse_starting_session_server(
            runtime_directory,
            config_directory,
            &mut router_sessions.session_registry,
            starting_session_server,
            "the session did not report a bound socket".to_string(),
        );
    }
    if find_earliest_description_due_at(router_sessions, now).is_some() {
        return;
    }
    for queued_session_creation in std::mem::take(&mut router_sessions.queued_session_creations) {
        if let Some(starting_session_server) = start_session_server(
            runtime_directory,
            router_sessions,
            router_events_sender,
            queued_session_creation,
        ) {
            router_sessions
                .starting_session_servers
                .push(starting_session_server);
        }
    }
}

/// The earliest moment [`DescriptionInFlight::find_due_at`] gives at `now`
/// across every description in flight. `None` when no description is due.
fn find_earliest_description_due_at(
    router_sessions: &RouterSessions,
    now: Instant,
) -> Option<Instant> {
    router_sessions
        .description_in_flight_by_session_id
        .values()
        .filter_map(|description_in_flight| description_in_flight.find_due_at(now))
        .min()
}

/// Start the session server `queued_session_creation` asks for, under a fresh
/// session id and a name no listed session and no starting session server
/// carries, and start the thread that reads its ready line.
///
/// The thread reports the line, or `None` for output that ends without a
/// readable one, as [`RouterEvent::SessionServerReported`], and sets
/// [`StartingSessionServer::is_ready_report_sent`] just before. The session
/// server has [`SESSION_SERVER_READY_TIMEOUT_DURATION`] to report.
///
/// Returns `None` once the creation is refused: the process cannot be
/// started, it has no readable output, or the reading thread cannot start. A
/// process that started is killed first.
fn start_session_server(
    runtime_directory: &Path,
    router_sessions: &RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    queued_session_creation: QueuedSessionCreation,
) -> Option<StartingSessionServer> {
    let session_id = SessionId::new();
    let session_name = generate_name(NameKind::Session, |candidate_session_name| {
        is_session_name_taken(router_sessions, candidate_session_name)
    });
    let refuse_creation = |refusal_message: String| {
        let _ = queued_session_creation
            .response_sender
            .send(build_refused_result(refusal_message));
    };

    let session_server_process = build_session_server_command(
        runtime_directory,
        session_id,
        &session_name,
        queued_session_creation.profile.as_deref(),
        queued_session_creation.working_directory.as_deref(),
        queued_session_creation.is_other_user_access_allowed,
    )
    .and_then(|mut session_server_command| session_server_command.spawn());
    let mut child_process = match session_server_process {
        Ok(child_process) => child_process,
        Err(session_start_error) => {
            refuse_creation(format!(
                "the session could not be started: {session_start_error}"
            ));
            return None;
        }
    };
    let Some(session_server_stdout) = child_process.stdout.take() else {
        terminate_child_process(&mut child_process);
        refuse_creation("the session server started without a readable output".to_string());
        return None;
    };
    let ready_events_sender = router_events_sender.clone();
    let is_ready_report_sent = Arc::new(AtomicBool::new(false));
    let is_reader_report_sent = Arc::clone(&is_ready_report_sent);
    let ready_report_reader_thread = std::thread::Builder::new()
        .name("koshi-router-ready".to_string())
        .spawn(move || {
            let ready_report = read_session_server_ready_line(session_server_stdout);
            is_reader_report_sent.store(true, Ordering::SeqCst);
            let _ = ready_events_sender.send(RouterEvent::SessionServerReported {
                session_id,
                ready_report,
            });
        });
    if let Err(ready_report_reader_error) = ready_report_reader_thread {
        terminate_child_process(&mut child_process);
        refuse_creation(format!(
            "the session could not be watched for startup: {ready_report_reader_error}"
        ));
        return None;
    }
    Some(StartingSessionServer {
        session_id,
        session_name,
        child_process,
        ready_deadline: Instant::now() + SESSION_SERVER_READY_TIMEOUT_DURATION,
        is_ready_report_sent,
        response_sender: queued_session_creation.response_sender,
    })
}

/// Register the session a starting session server reported, and answer its
/// creation with where it listens.
///
/// A report [`validate_session_server_ready`] accepts registers the session
/// with a reaper thread on its process, and answers [`RouterResult::Created`].
/// Any other report kills the session server, removes the files it
/// advertised, and refuses the creation with the reason. A report for a
/// session server no longer starting, one whose deadline passed first, changes
/// nothing.
///
/// `config_directory` goes to [`refuse_starting_session_server`].
fn finish_session_creation(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    session_id: SessionId,
    ready_report: Option<SessionServerReady>,
) {
    let Some(starting_index) = router_sessions
        .starting_session_servers
        .iter()
        .position(|starting_session_server| starting_session_server.session_id == session_id)
    else {
        return;
    };
    let starting_session_server = router_sessions
        .starting_session_servers
        .remove(starting_index);
    let ready_report = match validate_session_server_ready(ready_report) {
        Ok(ready_report) => ready_report,
        Err(ready_report_error) => {
            refuse_starting_session_server(
                runtime_directory,
                config_directory,
                &mut router_sessions.session_registry,
                starting_session_server,
                ready_report_error,
            );
            return;
        }
    };
    let process_id = starting_session_server.child_process.id();
    let has_exit_watcher = start_session_reaper_thread(
        starting_session_server.child_process,
        session_id,
        router_events_sender.clone(),
    );
    #[cfg(unix)]
    if !has_exit_watcher {
        router_sessions
            .unwaited_child_process_ids
            .insert(process_id);
    }
    router_sessions.session_registry.insert(
        session_id,
        SessionRecord {
            session_name: starting_session_server.session_name.clone(),
            socket_address: ready_report.socket_address.clone(),
            process_id,
            has_exit_watcher,
        },
    );
    let _ = starting_session_server
        .response_sender
        .send(RouterResult::Created(SessionAddress {
            session_id,
            session_name: starting_session_server.session_name,
            socket_address: ready_report.socket_address,
            process_id,
        }));
}

/// Kill a starting session server, remove the files it advertised through
/// [`remove_session_files`], and refuse its creation with `refusal_message`. A
/// session server that bound its socket before it was killed left an endpoint
/// file behind: that file, and what it advertised in the shared directory, are
/// removed.
///
/// `config_directory` goes to [`remove_session_files`].
fn refuse_starting_session_server(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    session_registry: &mut SessionRegistry,
    mut starting_session_server: StartingSessionServer,
    refusal_message: String,
) {
    terminate_child_process(&mut starting_session_server.child_process);
    let advertised_endpoint_file =
        load_session_endpoint_file(runtime_directory, starting_session_server.session_id)
            .ok()
            .flatten();
    remove_session_files(
        runtime_directory,
        config_directory,
        session_registry,
        starting_session_server.session_id,
        advertised_endpoint_file.as_ref(),
    );
    let _ = starting_session_server
        .response_sender
        .send(build_refused_result(refusal_message));
}

/// Take one attach lookup: ask every advertised session the list does not
/// hold to describe itself, through [`start_unlisted_session_descriptions`],
/// and join [`RouterSessions::waiting_attach_lookups`]. The dispatcher moves it
/// on through [`advance_attach_lookup`] on its next turn.
///
/// `foreign_session_listing` holds the sessions other local users started that
/// [`list_foreign_sessions_for_lookup`] found for `session_selector`. Its
/// unlisted count rides on the lookup, and so does each unread path: its own,
/// and `runtime_directory` when this user's sessions cannot be listed. Then
/// no session of this user's is asked.
fn start_attach_lookup(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    foreign_session_listing: ForeignSessionListing,
    session_selector: SessionSelector,
    response_sender: Sender<RouterResult>,
) {
    let unlisted_session_count = foreign_session_listing.unlisted_session_count;
    let mut unread_paths = Vec::new();
    let own_session_ids = match ipc_client::list_own_sessions(runtime_directory) {
        Ok(own_session_ids) => own_session_ids,
        Err(unread_path) => {
            unread_paths.push(unread_path);
            Vec::new()
        }
    };
    if let Some(unread_path) = &foreign_session_listing.unread_path {
        if !unread_paths.contains(unread_path) {
            unread_paths.push(unread_path.clone());
        }
    }
    let surveyed_session_ids = start_unlisted_session_descriptions(
        runtime_directory,
        router_sessions,
        router_events_sender,
        own_session_ids,
        foreign_session_listing,
    );
    let awaited_session_ids = match &session_selector {
        SessionSelector::SessionId(selected_session_id) => surveyed_session_ids
            .into_iter()
            .filter(|session_id| session_id == selected_session_id)
            .collect(),
        SessionSelector::SessionName(_) => surveyed_session_ids,
    };
    router_sessions
        .waiting_attach_lookups
        .push(WaitingAttachLookup {
            session_selector,
            awaited_session_ids,
            unlisted_session_count,
            unread_paths,
            answer_deadline: Instant::now() + SESSION_DISCOVERY_TIMEOUT_DURATION,
            probed_session_id: None,
            response_sender,
        });
}

/// Move one waiting attach lookup on at `now`.
///
/// - A probe it waits for still runs: it keeps waiting. The probe's answer
///   reaches it through [`hand_probe_verdict_to_attach_lookups`].
/// - Its selector names a session of this user's, through
///   [`resolve_session_selector`]: a probe of that session starts through
///   [`start_session_probe`], and the lookup waits for it. A probe already
///   running is waited for instead. A probe that cannot start refuses the
///   lookup with `the session could not be checked: <failure>`.
/// - Otherwise, while [`find_description_wait_end`] says it waits for a
///   description, it keeps waiting. A session of this user's that answers
///   meanwhile wins over a session another local user started.
/// - Otherwise a selector naming one session another local user started
///   probes that session the same way. A [`SessionSelector::SessionName`]
///   that names it while [`count_unanswered_sessions`] counts any session, or
///   while the lookup holds an unread path, is refused instead: ``cannot tell
///   whether `quiet-lake` is unique (1 running session did not answer)``, with
///   the clause [`format_lookup_gaps`] gives. A name several such sessions carry is refused
///   through [`build_ambiguous_name_result`]. A selector naming no listed
///   session is answered through [`build_unlisted_session_result`].
///
/// Returns the lookup while it still waits, and `None` once it is answered.
fn advance_attach_lookup(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    mut attach_lookup: WaitingAttachLookup,
    now: Instant,
) -> Option<WaitingAttachLookup> {
    if attach_lookup.probed_session_id.is_some() {
        return Some(attach_lookup);
    }
    let session_selection = resolve_session_selector(
        &router_sessions.session_registry,
        &attach_lookup.session_selector,
    );
    let is_waiting_for_description = !matches!(session_selection, SessionSelection::ThisUser(_))
        && find_description_wait_end(
            router_sessions,
            &attach_lookup.awaited_session_ids,
            attach_lookup.answer_deadline,
            now,
        )
        .is_some();
    if is_waiting_for_description {
        return Some(attach_lookup);
    }
    let session_id = match session_selection {
        SessionSelection::ThisUser(session_id) => session_id,
        SessionSelection::OtherUser(session_id) => {
            let unanswered_session_count =
                count_unanswered_sessions(router_sessions, &attach_lookup);
            match &attach_lookup.session_selector {
                SessionSelector::SessionName(session_name)
                    if unanswered_session_count > 0 || !attach_lookup.unread_paths.is_empty() =>
                {
                    let _ = attach_lookup
                        .response_sender
                        .send(build_refused_result(format!(
                            "cannot tell whether `{session_name}` is unique ({})",
                            format_lookup_gaps(
                                unanswered_session_count,
                                &attach_lookup.unread_paths
                            )
                        )));
                    return None;
                }
                _ => session_id,
            }
        }
        SessionSelection::AmbiguousName { session_ids } => {
            let _ = attach_lookup
                .response_sender
                .send(build_ambiguous_name_result(&session_ids));
            return None;
        }
        SessionSelection::NotListed => {
            let _ = attach_lookup
                .response_sender
                .send(build_unlisted_session_result(
                    router_sessions,
                    &attach_lookup,
                ));
            return None;
        }
    };
    if let Err(probe_start_error) = start_session_probe(
        runtime_directory,
        router_sessions,
        session_id,
        router_events_sender,
    ) {
        let _ = attach_lookup
            .response_sender
            .send(build_refused_result(format!(
                "the session could not be checked: {probe_start_error}"
            )));
        return None;
    }
    attach_lookup.probed_session_id = Some(session_id);
    Some(attach_lookup)
}

/// Move one waiting remote attach on at `now`. While
/// [`find_description_wait_end`] says it waits for a description, it keeps
/// waiting, whatever its selector names. Otherwise it is answered through
/// [`locate_remote_session`].
///
/// Returns the request while it still waits, and `None` once it is answered.
fn advance_remote_locate(
    runtime_directory: &Path,
    router_sessions: &RouterSessions,
    remote_state: &RemoteState,
    remote_locate: WaitingRemoteLocate,
    now: Instant,
) -> Option<WaitingRemoteLocate> {
    if find_description_wait_end(
        router_sessions,
        &remote_locate.awaited_session_ids,
        remote_locate.answer_deadline,
        now,
    )
    .is_some()
    {
        return Some(remote_locate);
    }
    let _ = remote_locate.response_sender.send(
        locate_remote_session(
            runtime_directory,
            &router_sessions.session_registry,
            remote_state,
            &remote_locate.scope,
            remote_locate.remote_connection_id,
            &remote_locate.session_selector,
        )
        .ok_or(LocateRefusal::NotReached),
    );
    None
}

/// The moment a request waiting for the descriptions of `awaited_session_ids`
/// must be looked at again, while one of them is still due at `now`, and
/// `None` once none is.
///
/// Each description's due moment comes from
/// [`DescriptionInFlight::find_due_at`]. An answer that is sent gives `now`,
/// whatever `answer_deadline` is: the request waits until that answer is
/// applied. Otherwise nothing is due at or past `answer_deadline`, and the
/// moment is the earliest due moment, and `answer_deadline` at the latest.
///
/// Example: a lookup arriving 1 second after another lookup asked a stopped
/// session waits 4 seconds for it, not 5.
fn find_description_wait_end(
    router_sessions: &RouterSessions,
    awaited_session_ids: &BTreeSet<SessionId>,
    answer_deadline: Instant,
    now: Instant,
) -> Option<Instant> {
    let earliest_due_at = awaited_session_ids
        .iter()
        .filter_map(|session_id| {
            router_sessions
                .description_in_flight_by_session_id
                .get(session_id)
        })
        .filter_map(|description_in_flight| description_in_flight.find_due_at(now))
        .min()?;
    if earliest_due_at <= now {
        return Some(now);
    }
    if now >= answer_deadline {
        return None;
    }
    Some(earliest_due_at.min(answer_deadline))
}

/// The answer to `attach_lookup`, whose selector names no listed session, once
/// it waits for nothing more.
///
/// - A [`SessionSelector::SessionId`] among its awaited sessions with a
///   reason from [`find_unanswered_session_reason`]: that reason, through
///   [`build_unanswered_session_result`], such as `session session-<uuid> is
///   running but did not answer: it did not answer within 5 seconds`.
/// - A [`SessionSelector::SessionId`] outside them while the lookup holds an
///   unread path: ``cannot tell whether session session-<uuid> is running
///   (<the clause [`format_lookup_gaps`] gives>)``.
/// - A [`SessionSelector::SessionName`] while the lookup holds an unread path:
///   ``no session named `quiet-lake` answered, and the names of some sessions
///   are unknown (<the clause [`format_lookup_gaps`] gives>)``.
/// - A [`SessionSelector::SessionName`] while [`count_unanswered_sessions`]
///   counts `N`: ``no session named `quiet-lake` answered; N running sessions
///   did not answer, so their names are unknown``. One session reads ``1
///   running session did not answer, so its name is unknown``.
/// - Anything else: [`build_session_not_found_result`].
fn build_unlisted_session_result(
    router_sessions: &RouterSessions,
    attach_lookup: &WaitingAttachLookup,
) -> RouterResult {
    let session_selector = &attach_lookup.session_selector;
    match session_selector {
        SessionSelector::SessionId(session_id) => {
            if !attach_lookup.awaited_session_ids.contains(session_id) {
                if !attach_lookup.unread_paths.is_empty() {
                    return build_refused_result(format!(
                        "cannot tell whether session {session_id} is running ({})",
                        format_lookup_gaps(0, &attach_lookup.unread_paths)
                    ));
                }
                return build_session_not_found_result(session_selector);
            }
            match find_unanswered_session_reason(router_sessions, *session_id) {
                Some(unanswered_session_reason) => {
                    build_unanswered_session_result(session_selector, unanswered_session_reason)
                }
                None => build_session_not_found_result(session_selector),
            }
        }
        SessionSelector::SessionName(session_name) => {
            let unanswered_session_count =
                count_unanswered_sessions(router_sessions, attach_lookup);
            if !attach_lookup.unread_paths.is_empty() {
                return build_refused_result(format!(
                    "no session named `{session_name}` answered, and the names of some sessions \
                     are unknown ({})",
                    format_lookup_gaps(unanswered_session_count, &attach_lookup.unread_paths)
                ));
            }
            match unanswered_session_count {
                0 => build_session_not_found_result(session_selector),
                1 => build_refused_result(format!(
                    "no session named `{session_name}` answered; 1 running session did not \
                     answer, so its name is unknown"
                )),
                unanswered_session_count => build_refused_result(format!(
                    "no session named `{session_name}` answered; {unanswered_session_count} \
                     running sessions did not answer, so their names are unknown"
                )),
            }
        }
    }
}

/// How many sessions `attach_lookup` cannot see the name of: each of its
/// awaited sessions with a reason from [`find_unanswered_session_reason`], and
/// each of its unlisted sessions.
fn count_unanswered_sessions(
    router_sessions: &RouterSessions,
    attach_lookup: &WaitingAttachLookup,
) -> usize {
    attach_lookup
        .awaited_session_ids
        .iter()
        .filter(|session_id| {
            find_unanswered_session_reason(router_sessions, **session_id).is_some()
        })
        .count()
        + attach_lookup.unlisted_session_count
}

/// What a lookup cannot see, as one clause: `1 running session did not
/// answer`, or `3 running sessions did not answer` for `3`, left out at `0`,
/// then each of `unread_paths`, all joined by `; `.
///
/// Example: `2` and an unread `/tmp/koshi/1002` give `2 running sessions did
/// not answer; /tmp/koshi/1002 could not be read: Input/output error (os error
/// 5)`.
fn format_lookup_gaps(unanswered_session_count: usize, unread_paths: &[UnreadPath]) -> String {
    let mut lookup_gap_clauses = Vec::new();
    match unanswered_session_count {
        0 => {}
        1 => lookup_gap_clauses.push("1 running session did not answer".to_string()),
        unanswered_session_count => lookup_gap_clauses.push(format!(
            "{unanswered_session_count} running sessions did not answer"
        )),
    }
    lookup_gap_clauses.extend(unread_paths.iter().map(UnreadPath::to_string));
    lookup_gap_clauses.join("; ")
}

/// Why the advertised session `session_id` is left out of the list, as the
/// text a lookup refusal names: `it did not answer within 5 seconds` while its
/// description has not answered, and the reason recorded in
/// [`RouterSessions::unanswered_reason_by_session_id`] otherwise. `None` for a
/// session that is listed, and for one with no reason recorded, such as one
/// that is gone.
fn find_unanswered_session_reason(
    router_sessions: &RouterSessions,
    session_id: SessionId,
) -> Option<String> {
    if router_sessions.session_registry.contains_key(&session_id) {
        return None;
    }
    if router_sessions
        .description_in_flight_by_session_id
        .contains_key(&session_id)
    {
        return Some(UnansweredSessionReason::NoAnswerInTime.to_string());
    }
    router_sessions
        .unanswered_reason_by_session_id
        .get(&session_id)
        .map(ToString::to_string)
}

/// Answer every attach lookup waiting on the probe of `session_id`, from what
/// [`apply_session_probe`] made of it.
///
/// - `Listening`: [`RouterResult::Found`] with the session's name, and the
///   address and process id its record holds.
/// - `NothingListening` with [`SessionRemoval::Removed`]:
///   [`build_session_not_found_result`].
/// - `NothingListening` with [`SessionRemoval::ReplacingItsImage`]:
///   [`build_unanswered_session_result`] with `it is restarting`.
/// - `NothingListening` with [`SessionRemoval::ProcessStillRunning`]:
///   [`build_unanswered_session_result`] with `process 5000 runs but accepts
///   no connection`.
/// - `NothingListening` with [`SessionRemoval::Rebound`]: a new probe starts,
///   and the lookup waits for it. A probe that cannot start refuses the lookup
///   with `the session could not be checked: <failure>`.
/// - `Unreachable`: `the session could not be reached: <failure>`.
/// - `EndpointFileUnreadable`: [`build_unanswered_session_result`] with the
///   read failure.
/// - `NotListed`: the lookup stops waiting on the probe, and the dispatcher's
///   next turn resolves its selector again.
fn hand_probe_verdict_to_attach_lookups(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    session_id: SessionId,
    session_probe_verdict: &SessionProbeVerdict,
) {
    for mut attach_lookup in std::mem::take(&mut router_sessions.waiting_attach_lookups) {
        if attach_lookup.probed_session_id != Some(session_id) {
            router_sessions.waiting_attach_lookups.push(attach_lookup);
            continue;
        }
        let router_result = match session_probe_verdict {
            SessionProbeVerdict::Listening => {
                let session_record = &router_sessions.session_registry[&session_id];
                RouterResult::Found(SessionAddress {
                    session_id,
                    session_name: session_record.session_name.clone(),
                    socket_address: session_record.socket_address.clone(),
                    process_id: session_record.process_id,
                })
            }
            SessionProbeVerdict::NothingListening(SessionRemoval::Removed) => {
                build_session_not_found_result(&attach_lookup.session_selector)
            }
            SessionProbeVerdict::NothingListening(SessionRemoval::ReplacingItsImage) => {
                build_unanswered_session_result(
                    &attach_lookup.session_selector,
                    UnansweredSessionReason::ReplacingItsImage,
                )
            }
            SessionProbeVerdict::NothingListening(SessionRemoval::ProcessStillRunning {
                process_id,
            }) => build_unanswered_session_result(
                &attach_lookup.session_selector,
                UnansweredSessionReason::ProcessStillRunning {
                    process_id: *process_id,
                },
            ),
            SessionProbeVerdict::Unreachable { connect_error_text } => build_refused_result(
                format!("the session could not be reached: {connect_error_text}"),
            ),
            SessionProbeVerdict::EndpointFileUnreadable {
                endpoint_file_error_text,
            } => build_unanswered_session_result(
                &attach_lookup.session_selector,
                endpoint_file_error_text,
            ),
            SessionProbeVerdict::NothingListening(SessionRemoval::Rebound) => {
                match start_session_probe(
                    runtime_directory,
                    router_sessions,
                    session_id,
                    router_events_sender,
                ) {
                    Ok(()) => {
                        router_sessions.waiting_attach_lookups.push(attach_lookup);
                        continue;
                    }
                    Err(probe_start_error) => build_refused_result(format!(
                        "the session could not be checked: {probe_start_error}"
                    )),
                }
            }
            SessionProbeVerdict::NotListed => {
                attach_lookup.probed_session_id = None;
                router_sessions.waiting_attach_lookups.push(attach_lookup);
                continue;
            }
        };
        let _ = attach_lookup.response_sender.send(router_result);
    }
}

/// Whether a failed description says the session is gone.
///
/// [`CliError::SessionNotFound`] is the only failure that means nothing is
/// listening on the session's socket. Every other failure —
/// [`CliError::IpcUnavailable`] for a settled protocol version outside this
/// build's range, a refusal, or an endpoint file this build cannot read — comes
/// from a session that is still bound and serving.
fn is_session_gone_error(cli_error: &CliError) -> bool {
    matches!(cli_error, CliError::SessionNotFound { .. })
}

/// The router's sessions at startup: an empty list, with every session already
/// running asked to describe itself.
///
/// [`remove_orphan_resume_files`] runs first. Then every advertised session
/// goes through [`start_unlisted_session_descriptions`]: the sessions this user
/// started, and, while `shared_sessions_base_directory` is given, the sessions
/// other local users started, as
/// [`list_foreign_sessions`](ipc_client::list_foreign_sessions) lists them.
/// Nothing is waited for: each answer registers its session once it reaches
/// the dispatcher, and a lookup that arrives first waits for it. A session
/// server that outlived an earlier router is registered this way.
fn create_router_sessions(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
    router_events_sender: &Sender<RouterEvent>,
) -> RouterSessions {
    remove_orphan_resume_files(runtime_directory);
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    start_unlisted_session_descriptions(
        runtime_directory,
        &mut router_sessions,
        router_events_sender,
        list_own_sessions_or_none(runtime_directory),
        shared_sessions_base_directory
            .map(|shared_sessions_base_directory| {
                ipc_client::list_foreign_sessions(shared_sessions_base_directory, runtime_directory)
            })
            .unwrap_or_default(),
    );
    router_sessions
}

/// This user's sessions, as [`list_own_sessions`](ipc_client::list_own_sessions)
/// gives them from `runtime_directory`. A runtime directory that cannot be read
/// gives none, and a warning naming it is logged.
fn list_own_sessions_or_none(runtime_directory: &Path) -> Vec<SessionId> {
    match ipc_client::list_own_sessions(runtime_directory) {
        Ok(own_session_ids) => own_session_ids,
        Err(unread_path) => {
            tracing::warn!(%unread_path, "this user's sessions could not be listed");
            Vec::new()
        }
    }
}

/// The sessions other local users started that an attach lookup of
/// `session_selector` asks, from the shared directory under
/// `shared_sessions_base_directory`. Nothing is read while
/// `shared_sessions_base_directory` is `None`, which it is while
/// `allow-other-users` is off.
///
/// - A [`SessionSelector::SessionId`] the list holds reads nothing.
/// - Any other [`SessionSelector::SessionId`] reads the one path
///   [`find_foreign_session_address`](ipc_client::find_foreign_session_address)
///   looks up, and gives that session when it finds it.
/// - A [`SessionSelector::SessionName`] that [`resolve_session_selector`]
///   resolves to a session of this user's reads nothing: that session wins.
/// - Any other [`SessionSelector::SessionName`] gives every session
///   [`list_foreign_sessions`](ipc_client::list_foreign_sessions) lists.
///
/// Example: `koshi attach session-<uuid>` for a listed session reads no folder
/// of the shared directory, and `koshi attach quiet-lake` with no listed
/// session of that name reads every one, up to the limits
/// [`list_foreign_sessions`](ipc_client::list_foreign_sessions) states.
///
/// # Errors
/// The failure [`find_foreign_session_address`](ipc_client::find_foreign_session_address)
/// gives, such as an id that sockets of two users advertise.
fn list_foreign_sessions_for_lookup(
    shared_sessions_base_directory: Option<&Path>,
    runtime_directory: &Path,
    session_registry: &SessionRegistry,
    session_selector: &SessionSelector,
) -> Result<ForeignSessionListing, ipc_client::ForeignSessionLookupError> {
    let Some(shared_sessions_base_directory) = shared_sessions_base_directory else {
        return Ok(ForeignSessionListing::default());
    };
    match session_selector {
        SessionSelector::SessionId(session_id) if session_registry.contains_key(session_id) => {
            Ok(ForeignSessionListing::default())
        }
        SessionSelector::SessionId(session_id) => Ok(ForeignSessionListing {
            foreign_sessions: ipc_client::find_foreign_session_address(
                shared_sessions_base_directory,
                runtime_directory,
                *session_id,
            )?
            .map(|socket_address| (*session_id, socket_address))
            .into_iter()
            .collect(),
            ..ForeignSessionListing::default()
        }),
        SessionSelector::SessionName(_) => {
            match resolve_session_selector(session_registry, session_selector) {
                SessionSelection::ThisUser(_) => Ok(ForeignSessionListing::default()),
                _ => Ok(ipc_client::list_foreign_sessions(
                    shared_sessions_base_directory,
                    runtime_directory,
                )),
            }
        }
    }
}

/// Ask every advertised session the list does not hold to describe itself,
/// and hand back the ids of the advertised sessions found unlisted.
///
/// The sessions are this user's, `own_session_ids`, as
/// [`list_own_sessions`](ipc_client::list_own_sessions) gives them from
/// `runtime_directory`, and the sessions other local users started that
/// `foreign_session_listing` holds. A session id this user advertises is never
/// taken for another user's. A session a starting session server holds is
/// skipped: its ready line registers it.
///
/// Per session of this user's:
///
/// - Its endpoint file is gone while [`is_replacing_its_image`] accepts it:
///   it is not asked, [`UnansweredSessionReason::ReplacingItsImage`] is
///   recorded for it, and it is handed back.
/// - Its endpoint file is gone otherwise: it is skipped and not handed back.
/// - Its endpoint file exists and cannot be read, such as one written with
///   fields this build does not know: it is not asked, its files stay, and
///   [`UnansweredSessionReason::EndpointFileUnreadable`] is recorded for it.
/// - Otherwise it is asked through [`start_session_description`].
///
/// Every other user's session is asked through [`start_session_description`]
/// at the address the shared directory names, while fewer than
/// [`MAX_OTHER_USER_DESCRIPTIONS_IN_FLIGHT`] descriptions of such sessions
/// run. One found past that is not asked, records
/// [`UnansweredSessionReason::TooManyOtherUserDescriptions`], and is handed
/// back. Each id more than one socket advertises is not asked, records
/// [`UnansweredSessionReason::AdvertisedMoreThanOnce`], and is handed back.
/// While [`unread_path`](ForeignSessionListing::unread_path) names a path the
/// listing could not read, no other user's session is asked, and none is
/// handed back. Example: a listing that stopped at the 257th user folder holds `session-X`
/// at `/tmp/koshi/1002/session-X.sock`, and `session-X` is not asked.
///
/// A recorded reason is dropped first when its session is in none of these
/// places and no waiting attach lookup or remote locate awaits it.
///
/// The walk is over endpoint files and resume files, which exist on every
/// platform: a Windows pipe with no directory entry of its own is found.
fn start_unlisted_session_descriptions(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    own_session_ids: Vec<SessionId>,
    foreign_session_listing: ForeignSessionListing,
) -> BTreeSet<SessionId> {
    let waiting_attach_lookups = &router_sessions.waiting_attach_lookups;
    let waiting_remote_locates = &router_sessions.waiting_remote_locates;
    router_sessions
        .unanswered_reason_by_session_id
        .retain(|session_id, _| {
            own_session_ids.contains(session_id)
                || foreign_session_listing
                    .foreign_sessions
                    .iter()
                    .any(|(foreign_session_id, _)| foreign_session_id == session_id)
                || foreign_session_listing
                    .duplicated_sessions
                    .iter()
                    .any(|duplicated_session| duplicated_session.session_id == *session_id)
                || waiting_attach_lookups
                    .iter()
                    .any(|attach_lookup| attach_lookup.awaited_session_ids.contains(session_id))
                || waiting_remote_locates
                    .iter()
                    .any(|remote_locate| remote_locate.awaited_session_ids.contains(session_id))
        });
    let mut surveyed_session_ids = BTreeSet::new();
    for session_id in own_session_ids {
        if is_session_listed_or_starting(router_sessions, session_id) {
            continue;
        }
        match load_session_endpoint_file(runtime_directory, session_id) {
            Ok(None) if is_replacing_its_image(runtime_directory, session_id) => {
                router_sessions
                    .unanswered_reason_by_session_id
                    .insert(session_id, UnansweredSessionReason::ReplacingItsImage);
            }
            Ok(None) => continue,
            Ok(Some(_)) => start_session_description(
                runtime_directory,
                router_sessions,
                router_events_sender,
                session_id,
                DescribedSessionOrigin::ThisUser,
            ),
            Err(endpoint_file_error) => {
                router_sessions.unanswered_reason_by_session_id.insert(
                    session_id,
                    UnansweredSessionReason::EndpointFileUnreadable {
                        endpoint_file_error,
                    },
                );
            }
        }
        surveyed_session_ids.insert(session_id);
    }
    for duplicated_session in foreign_session_listing.duplicated_sessions {
        if is_session_listed_or_starting(router_sessions, duplicated_session.session_id) {
            continue;
        }
        surveyed_session_ids.insert(duplicated_session.session_id);
        router_sessions.unanswered_reason_by_session_id.insert(
            duplicated_session.session_id,
            UnansweredSessionReason::AdvertisedMoreThanOnce { duplicated_session },
        );
    }
    if foreign_session_listing.unread_path.is_some() {
        return surveyed_session_ids;
    }
    for (foreign_session_id, foreign_socket_address) in foreign_session_listing.foreign_sessions {
        if is_session_listed_or_starting(router_sessions, foreign_session_id) {
            continue;
        }
        surveyed_session_ids.insert(foreign_session_id);
        let other_user_description_count = router_sessions
            .description_in_flight_by_session_id
            .values()
            .filter(|description_in_flight| description_in_flight.is_other_user_session)
            .count();
        if other_user_description_count >= MAX_OTHER_USER_DESCRIPTIONS_IN_FLIGHT
            && !router_sessions
                .description_in_flight_by_session_id
                .contains_key(&foreign_session_id)
        {
            router_sessions.unanswered_reason_by_session_id.insert(
                foreign_session_id,
                UnansweredSessionReason::TooManyOtherUserDescriptions,
            );
            continue;
        }
        start_session_description(
            runtime_directory,
            router_sessions,
            router_events_sender,
            foreign_session_id,
            DescribedSessionOrigin::OtherUser {
                socket_address: foreign_socket_address,
            },
        );
    }
    surveyed_session_ids
}

/// Whether `session_id` is listed, or held by a session server that started
/// and has not reported.
fn is_session_listed_or_starting(router_sessions: &RouterSessions, session_id: SessionId) -> bool {
    router_sessions.session_registry.contains_key(&session_id)
        || router_sessions
            .starting_session_servers
            .iter()
            .any(|starting_session_server| starting_session_server.session_id == session_id)
}

/// Ask `session_id` to describe itself at `described_session_origin`, on a
/// thread of its own, and record the description in
/// [`RouterSessions::description_in_flight_by_session_id`].
///
/// A session already being asked is not asked again. Asking clears the reason
/// recorded for the session. The thread reports through
/// [`fetch_session_description`] as [`RouterEvent::SessionDescribed`], with an
/// answer deadline [`SESSION_DISCOVERY_TIMEOUT_DURATION`] after the session
/// was asked, and sets [`DescriptionInFlight::is_answer_sent`] just before it
/// sends. A thread that cannot start records
/// [`UnansweredSessionReason::DescriptionFailed`] naming the failure.
///
/// Example: a session stopped with `SIGSTOP` is asked once, and that thread
/// ends with [`CliError::SessionAnswerTimedOut`] 5 seconds after the ask.
fn start_session_description(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    session_id: SessionId,
    described_session_origin: DescribedSessionOrigin,
) {
    if router_sessions
        .description_in_flight_by_session_id
        .contains_key(&session_id)
    {
        return;
    }
    router_sessions
        .unanswered_reason_by_session_id
        .remove(&session_id);
    let asked_at = Instant::now();
    let is_other_user_session = matches!(
        described_session_origin,
        DescribedSessionOrigin::OtherUser { .. }
    );
    let is_answer_sent = Arc::new(AtomicBool::new(false));
    let is_thread_answer_sent = Arc::clone(&is_answer_sent);
    let described_runtime_directory = runtime_directory.to_path_buf();
    let description_events_sender = router_events_sender.clone();
    let describing_thread = std::thread::Builder::new()
        .name("koshi-router-discovery".to_string())
        .spawn(move || {
            let session_described = fetch_session_description(
                &described_runtime_directory,
                session_id,
                described_session_origin,
                asked_at + SESSION_DISCOVERY_TIMEOUT_DURATION,
            );
            is_thread_answer_sent.store(true, Ordering::SeqCst);
            let _ = description_events_sender.send(session_described);
        });
    match describing_thread {
        Ok(_) => {
            router_sessions.description_in_flight_by_session_id.insert(
                session_id,
                DescriptionInFlight {
                    asked_at,
                    is_other_user_session,
                    is_answer_sent,
                },
            );
        }
        Err(thread_start_error) => {
            router_sessions.unanswered_reason_by_session_id.insert(
                session_id,
                UnansweredSessionReason::DescriptionFailed {
                    description_error: CliError::Runtime {
                        detail: format!(
                            "the router could not start a thread to ask it: {thread_start_error}"
                        ),
                    },
                },
            );
        }
    }
}

/// Ask `session_id` to describe itself at `described_session_origin`, and
/// build the [`RouterEvent::SessionDescribed`] that reports the answer.
///
/// For a session of this user's, its endpoint file in `runtime_directory` is
/// read first, and the session is asked at the address and with the token it
/// names. An endpoint file that is gone answers [`CliError::SessionNotFound`];
/// one that cannot be read answers [`CliError::IpcUnavailable`] naming the
/// failure. A refused connect is made again through
/// [`repeat_while_live_session_refuses`], which reads the endpoint file again
/// each time. Another user's session is asked at the address the origin names.
///
/// Every connect and exchange ends by `answer_deadline`, and so does the
/// recheck of a refused connect. An exchange still unfinished then answers
/// [`CliError::SessionAnswerTimedOut`].
fn fetch_session_description(
    runtime_directory: &Path,
    session_id: SessionId,
    described_session_origin: DescribedSessionOrigin,
    answer_deadline: Instant,
) -> RouterEvent {
    let (described_endpoint_file, description_answer) = match &described_session_origin {
        DescribedSessionOrigin::OtherUser { socket_address } => (
            None,
            ipc_client::fetch_foreign_session_overview(session_id, socket_address, answer_deadline),
        ),
        DescribedSessionOrigin::ThisUser => repeat_while_live_session_refuses(
            runtime_directory,
            session_id,
            Some(answer_deadline),
            || fetch_own_session_description(runtime_directory, session_id, answer_deadline),
            |(_, description_answer)| {
                matches!(description_answer, Err(CliError::SessionNotFound { .. }))
            },
        ),
    };
    RouterEvent::SessionDescribed {
        session_id,
        described_session_origin,
        described_endpoint_file,
        description_answer,
    }
}

/// Read the endpoint file of the session `session_id` of this user's in
/// `runtime_directory`, and ask the session at the address and with the token
/// it names, ending the exchange by `answer_deadline`. Hands back the endpoint
/// file read, and the answer. An endpoint file that is gone answers
/// [`CliError::SessionNotFound`]; one that cannot be read answers
/// [`CliError::IpcUnavailable`] naming the failure.
fn fetch_own_session_description(
    runtime_directory: &Path,
    session_id: SessionId,
    answer_deadline: Instant,
) -> (Option<EndpointFile>, Result<SessionOverview, CliError>) {
    match load_session_endpoint_file(runtime_directory, session_id) {
        Ok(Some(endpoint_file)) => {
            let description_answer = ipc_client::fetch_session_overview_from_endpoint(
                &endpoint_file,
                session_id,
                Some(answer_deadline),
            );
            (Some(endpoint_file), description_answer)
        }
        Ok(None) => (
            None,
            Err(CliError::SessionNotFound {
                session_name: session_id.to_string(),
            }),
        ),
        Err(endpoint_file_error) => (
            None,
            Err(CliError::IpcUnavailable {
                detail: endpoint_file_error.to_string(),
            }),
        ),
    }
}

/// Apply the answer `session_id` gave to its description to the list.
///
/// The session stops counting as asked first. A session the list holds, and
/// one a starting session server holds, change nothing more. The endpoint file
/// of `session_id` in `runtime_directory` is read again next: one that exists
/// and cannot be read records [`UnansweredSessionReason::EndpointFileUnreadable`]
/// and changes nothing else.
///
/// A session another local user started, asked at the address the shared
/// directory names:
///
/// - This user has an endpoint file of that id: nothing changes. This user's
///   session holds the id.
/// - An overview: the session is registered with that overview's name, that
///   address, process id `0`, and no exit watcher.
/// - A failure [`is_session_gone_error`] accepts: nothing changes. Its files
///   belong to that user.
/// - Any other failure records the reason
///   [`build_description_failure_reason`] gives.
///
/// A session of this user's whose endpoint file is gone, whatever its answer,
/// goes through [`apply_session_removal`] expecting no endpoint file, which
/// removes its resume file and socket file. Otherwise, by answer:
///
/// - An overview: the session is registered with that overview's name and the
///   address and process id its endpoint file names now. On Unix, the record
///   has an exit watcher when that process id is in
///   [`RouterSessions::inherited_child_process_ids`]: the thread
///   [`adopt_inherited_children`] started reports its exit. Any other session
///   has none.
/// - A failure [`is_session_gone_error`] accepts: the session goes through
///   [`apply_session_removal`] expecting `described_endpoint_file`, files
///   included.
/// - Any other failure, such as a refused Hello or a protocol version outside
///   this build's range: its files stay, and the reason
///   [`build_description_failure_reason`] gives is recorded.
///
/// `config_directory` goes to [`remove_session_from_registry`].
#[allow(clippy::too_many_arguments)]
fn apply_session_description(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    session_id: SessionId,
    described_session_origin: &DescribedSessionOrigin,
    described_endpoint_file: Option<&EndpointFile>,
    description_answer: Result<SessionOverview, CliError>,
) {
    router_sessions
        .description_in_flight_by_session_id
        .remove(&session_id);
    if is_session_listed_or_starting(router_sessions, session_id) {
        return;
    }
    let current_endpoint_file = match load_session_endpoint_file(runtime_directory, session_id) {
        Ok(current_endpoint_file) => current_endpoint_file,
        Err(endpoint_file_error) => {
            router_sessions.unanswered_reason_by_session_id.insert(
                session_id,
                UnansweredSessionReason::EndpointFileUnreadable {
                    endpoint_file_error,
                },
            );
            return;
        }
    };
    let unanswered_session_reason = match (described_session_origin, current_endpoint_file) {
        (DescribedSessionOrigin::OtherUser { .. }, Some(_)) => return,
        (DescribedSessionOrigin::ThisUser, None) => {
            let Some(unanswered_session_reason) = apply_session_removal(
                runtime_directory,
                config_directory,
                router_sessions,
                router_events_sender,
                session_id,
                None,
            ) else {
                return;
            };
            unanswered_session_reason
        }
        (DescribedSessionOrigin::OtherUser { socket_address }, None) => match description_answer {
            Ok(session_overview) => {
                router_sessions.session_registry.insert(
                    session_id,
                    SessionRecord {
                        session_name: session_overview.session.session_name,
                        socket_address: socket_address.clone(),
                        process_id: 0,
                        has_exit_watcher: false,
                    },
                );
                return;
            }
            Err(description_error) if is_session_gone_error(&description_error) => return,
            Err(description_error) => build_description_failure_reason(description_error),
        },
        (DescribedSessionOrigin::ThisUser, Some(current_endpoint_file)) => match description_answer
        {
            Ok(session_overview) => {
                #[cfg(unix)]
                let has_exit_watcher = router_sessions
                    .inherited_child_process_ids
                    .contains(&current_endpoint_file.process_id);
                #[cfg(not(unix))]
                let has_exit_watcher = false;
                router_sessions.session_registry.insert(
                    session_id,
                    SessionRecord {
                        session_name: session_overview.session.session_name,
                        socket_address: current_endpoint_file.socket_address,
                        process_id: current_endpoint_file.process_id,
                        has_exit_watcher,
                    },
                );
                return;
            }
            Err(description_error) if is_session_gone_error(&description_error) => {
                let Some(unanswered_session_reason) = apply_session_removal(
                    runtime_directory,
                    config_directory,
                    router_sessions,
                    router_events_sender,
                    session_id,
                    described_endpoint_file,
                ) else {
                    return;
                };
                unanswered_session_reason
            }
            Err(description_error) => build_description_failure_reason(description_error),
        },
    };
    router_sessions
        .unanswered_reason_by_session_id
        .insert(session_id, unanswered_session_reason);
}

/// Remove the session `session_id` of this user's that nothing answered for
/// through [`remove_session_from_registry`], expecting `expected_endpoint_file`
/// on disk, and hand back the reason a session that removal leaves in place
/// records.
///
/// - [`SessionRemoval::Removed`] gives `None`.
/// - [`SessionRemoval::Rebound`] asks the session again through
///   [`start_session_description`], and gives `None`.
/// - [`SessionRemoval::ReplacingItsImage`] gives
///   [`UnansweredSessionReason::ReplacingItsImage`].
/// - [`SessionRemoval::ProcessStillRunning`] gives
///   [`UnansweredSessionReason::ProcessStillRunning`] with its process id.
///
/// `config_directory` goes to [`remove_session_from_registry`].
fn apply_session_removal(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    session_id: SessionId,
    expected_endpoint_file: Option<&EndpointFile>,
) -> Option<UnansweredSessionReason> {
    match remove_session_from_registry(
        runtime_directory,
        config_directory,
        &mut router_sessions.session_registry,
        session_id,
        expected_endpoint_file,
    ) {
        SessionRemoval::Removed => None,
        SessionRemoval::Rebound => {
            start_session_description(
                runtime_directory,
                router_sessions,
                router_events_sender,
                session_id,
                DescribedSessionOrigin::ThisUser,
            );
            None
        }
        SessionRemoval::ReplacingItsImage => Some(UnansweredSessionReason::ReplacingItsImage),
        SessionRemoval::ProcessStillRunning { process_id } => {
            Some(UnansweredSessionReason::ProcessStillRunning { process_id })
        }
    }
}

/// The reason a description that failed with `description_error` records:
/// [`UnansweredSessionReason::NoAnswerInTime`] for
/// [`CliError::SessionAnswerTimedOut`], and
/// [`UnansweredSessionReason::DescriptionFailed`] naming the failure
/// otherwise.
///
/// Example: a session that accepts the connection and never answers gives
/// `it did not answer within 5 seconds`, the same reason a lookup reports
/// while the description still runs.
fn build_description_failure_reason(description_error: CliError) -> UnansweredSessionReason {
    match description_error {
        CliError::SessionAnswerTimedOut => UnansweredSessionReason::NoAnswerInTime,
        description_error => UnansweredSessionReason::DescriptionFailed { description_error },
    }
}

/// The endpoint file of `session_id` in `runtime_directory`, or `Ok(None)` when
/// there is none.
///
/// # Errors
/// Returns the failure of an endpoint file that exists and cannot be read,
/// such as one written with fields this build does not know.
fn load_session_endpoint_file(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Result<Option<EndpointFile>, IpcError> {
    match EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    )) {
        Ok(endpoint_file) => Ok(Some(endpoint_file)),
        Err(IpcError::EndpointFileMissing { .. }) => Ok(None),
        Err(endpoint_file_error) => Err(endpoint_file_error),
    }
}

/// Start a probe of every listed session whose `has_exit_watcher` is `false`,
/// through [`start_session_probe`]. A session with an exit watcher is not
/// probed. A probe that cannot start is tried again in the next round.
fn start_unwatched_session_probes(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
) {
    let unwatched_session_ids: Vec<SessionId> = router_sessions
        .session_registry
        .iter()
        .filter(|(_, session_record)| !session_record.has_exit_watcher)
        .map(|(session_id, _)| *session_id)
        .collect();
    for session_id in unwatched_session_ids {
        let _ = start_session_probe(
            runtime_directory,
            router_sessions,
            session_id,
            router_events_sender,
        );
    }
}

/// Probe the address of the listed session `session_id` on a thread of its
/// own, through [`probe_session_address`], which reports as
/// [`RouterEvent::SessionProbed`]. The address handed to the thread is the
/// one the session's record holds. A session already being probed starts
/// nothing.
///
/// # Errors
/// Returns the failure of a thread that could not start. Nothing is recorded.
///
/// # Panics
/// Panics when the list does not hold `session_id`.
fn start_session_probe(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    session_id: SessionId,
    router_events_sender: &Sender<RouterEvent>,
) -> std::io::Result<()> {
    if router_sessions.probing_session_ids.contains(&session_id) {
        return Ok(());
    }
    let listed_socket_address = router_sessions.session_registry[&session_id]
        .socket_address
        .clone();
    let probed_runtime_directory = runtime_directory.to_path_buf();
    let probe_events_sender = router_events_sender.clone();
    std::thread::Builder::new()
        .name("koshi-router-probe".to_string())
        .spawn(move || {
            let _ = probe_events_sender.send(probe_session_address(
                &probed_runtime_directory,
                session_id,
                listed_socket_address,
            ));
        })?;
    router_sessions.probing_session_ids.insert(session_id);
    Ok(())
}

/// Probe the session `session_id`, and build the [`RouterEvent::SessionProbed`]
/// that reports what the probe found.
///
/// The probe goes through [`probe_session_address_once`], and a probe that
/// finds nothing listening is made again through
/// [`repeat_while_live_session_refuses`].
fn probe_session_address(
    runtime_directory: &Path,
    session_id: SessionId,
    listed_socket_address: String,
) -> RouterEvent {
    repeat_while_live_session_refuses(
        runtime_directory,
        session_id,
        None,
        || probe_session_address_once(runtime_directory, session_id, &listed_socket_address),
        |session_probed| {
            matches!(
                session_probed,
                RouterEvent::SessionProbed {
                    session_probe_outcome: SessionProbeOutcome::NoListener,
                    ..
                }
            )
        },
    )
}

/// Probe the session `session_id` once, and build the
/// [`RouterEvent::SessionProbed`] that reports what the probe found.
///
/// The endpoint file of `session_id` in `runtime_directory` is read first, and
/// the address it names is probed. With no endpoint file, as for a session
/// another local user started, `listed_socket_address` is probed. The probe
/// opens one connection, sends nothing, and closes it: the session server's
/// serving thread reads end of stream and returns. An endpoint file that
/// exists and cannot be read reports
/// [`SessionProbeOutcome::EndpointFileUnreadable`], and nothing is probed.
fn probe_session_address_once(
    runtime_directory: &Path,
    session_id: SessionId,
    listed_socket_address: &str,
) -> RouterEvent {
    let probed_endpoint_file = match load_session_endpoint_file(runtime_directory, session_id) {
        Ok(probed_endpoint_file) => probed_endpoint_file,
        Err(endpoint_file_error) => {
            return RouterEvent::SessionProbed {
                session_id,
                probed_endpoint_file: None,
                session_probe_outcome: SessionProbeOutcome::EndpointFileUnreadable {
                    endpoint_file_error,
                },
            }
        }
    };
    let probed_socket_address = match &probed_endpoint_file {
        Some(probed_endpoint_file) => probed_endpoint_file.socket_address.clone(),
        None => listed_socket_address.to_string(),
    };
    let session_probe_outcome = match Connection::connect(&probed_socket_address) {
        Ok(_probe_connection) => SessionProbeOutcome::Listening,
        Err(IpcError::NoListener { .. }) => SessionProbeOutcome::NoListener,
        Err(connect_error) => SessionProbeOutcome::Unreachable { connect_error },
    };
    RouterEvent::SessionProbed {
        session_id,
        probed_endpoint_file,
        session_probe_outcome,
    }
}

/// Apply what one probe of `session_id` found to the list, and hand back the
/// verdict the lookups waiting on that probe answer from.
///
/// The session stops counting as probed first. A session the list does not
/// hold gives [`SessionProbeVerdict::NotListed`]. Otherwise, by outcome:
///
/// - Listening: the record takes the address and process id
///   `probed_endpoint_file` names, when there is one.
/// - Nothing listening: the session goes through
///   [`remove_session_from_registry`] expecting `probed_endpoint_file`, and
///   [`SessionProbeVerdict::NothingListening`] carries what that did.
/// - Any other connect failure, and an endpoint file that cannot be read,
///   change nothing.
///
/// Example: a session listed at `/run/user/1000/koshi/session-<uuid>.sock`
/// restarts with `allow-other-users` turned on, and its endpoint file now names
/// `/srv/koshi-shared/1000/session-<uuid>.sock`. A probe that read the old file
/// and found nothing at the old address gives `NothingListening` with
/// [`SessionRemoval::Rebound`], and nothing is removed.
///
/// `config_directory` goes to [`remove_session_from_registry`].
fn apply_session_probe(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    router_sessions: &mut RouterSessions,
    session_id: SessionId,
    probed_endpoint_file: Option<&EndpointFile>,
    session_probe_outcome: SessionProbeOutcome,
) -> SessionProbeVerdict {
    router_sessions.probing_session_ids.remove(&session_id);
    let Some(session_record) = router_sessions.session_registry.get_mut(&session_id) else {
        return SessionProbeVerdict::NotListed;
    };
    match session_probe_outcome {
        SessionProbeOutcome::Listening => {
            if let Some(probed_endpoint_file) = probed_endpoint_file {
                session_record
                    .socket_address
                    .clone_from(&probed_endpoint_file.socket_address);
                session_record.process_id = probed_endpoint_file.process_id;
            }
            SessionProbeVerdict::Listening
        }
        SessionProbeOutcome::Unreachable { connect_error } => SessionProbeVerdict::Unreachable {
            connect_error_text: connect_error.to_string(),
        },
        SessionProbeOutcome::EndpointFileUnreadable {
            endpoint_file_error,
        } => SessionProbeVerdict::EndpointFileUnreadable {
            endpoint_file_error_text: endpoint_file_error.to_string(),
        },
        SessionProbeOutcome::NoListener => {
            SessionProbeVerdict::NothingListening(remove_session_from_registry(
                runtime_directory,
                config_directory,
                &mut router_sessions.session_registry,
                session_id,
                probed_endpoint_file,
            ))
        }
    }
}

/// True when a listed session, or a session server started and not yet
/// reported, already carries `candidate_session_name` as its name.
fn is_session_name_taken(router_sessions: &RouterSessions, candidate_session_name: &str) -> bool {
    router_sessions
        .session_registry
        .values()
        .any(|session_record| session_record.session_name == candidate_session_name)
        || router_sessions
            .starting_session_servers
            .iter()
            .any(|starting_session_server| {
                starting_session_server.session_name == candidate_session_name
            })
}

/// Which listed session `session_selector` names.
///
/// A [`SessionSelector::SessionId`] names that session. A
/// [`SessionSelector::SessionName`] names every session whose name matches in
/// full. Among the sessions named, a session of this user's wins, by the rule
/// [`is_session_of_this_user`] states, and the lowest id wins among several of
/// them. With no session of this user's named, one session another local user
/// started is [`SessionSelection::OtherUser`], and several are
/// [`SessionSelection::AmbiguousName`].
///
/// Example: this user's `S-quiet-lake` and another user's `S-quiet-lake` are
/// both listed. The name gives [`SessionSelection::ThisUser`] with this user's
/// session id.
fn resolve_session_selector(
    session_registry: &SessionRegistry,
    session_selector: &SessionSelector,
) -> SessionSelection {
    let mut this_user_session_ids = Vec::new();
    let mut other_user_session_ids = Vec::new();
    for (session_id, session_record) in session_registry {
        let is_named = match session_selector {
            SessionSelector::SessionId(selected_session_id) => session_id == selected_session_id,
            SessionSelector::SessionName(session_name) => {
                session_record.session_name == *session_name
            }
        };
        if !is_named {
            continue;
        }
        if is_session_of_this_user(session_record) {
            this_user_session_ids.push(*session_id);
        } else {
            other_user_session_ids.push(*session_id);
        }
    }
    if let Some(session_id) = this_user_session_ids.into_iter().min() {
        return SessionSelection::ThisUser(session_id);
    }
    other_user_session_ids.sort();
    match other_user_session_ids.as_slice() {
        [] => SessionSelection::NotListed,
        [session_id] => SessionSelection::OtherUser(*session_id),
        _ => SessionSelection::AmbiguousName {
            session_ids: other_user_session_ids,
        },
    }
}

/// Drop one session that nothing answered for from the list, and remove every
/// file it left through [`remove_session_files`], once nothing says it is
/// still there. `expected_endpoint_file` is the endpoint file the caller read
/// before it found nothing listening, or `None` when there was none.
///
/// Nothing changes, checked in this order:
///
/// 1. A session that is replacing its own process image gives
///    [`SessionRemoval::ReplacingItsImage`]. Its socket is unbound for that
///    moment, and its new image rebinds the socket and rewrites the endpoint
///    file. Past that window its resume file goes with the rest.
/// 2. The endpoint file is read again. One that cannot be read, or that holds
///    anything but `expected_endpoint_file`, gives
///    [`SessionRemoval::Rebound`]. Example: a new image wrote a new connection
///    token after the caller read the old file.
/// 3. A process [`is_refusal_from_live_session`] accepts gives
///    [`SessionRemoval::ProcessStillRunning`]. Example: on macOS a session at
///    process `5000` with a full listen queue refuses a connect, and stays
///    listed.
///
/// Otherwise the session is removed, and this gives [`SessionRemoval::Removed`].
///
/// `config_directory` goes to [`remove_session_files`].
fn remove_session_from_registry(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    session_registry: &mut SessionRegistry,
    session_id: SessionId,
    expected_endpoint_file: Option<&EndpointFile>,
) -> SessionRemoval {
    if is_replacing_its_image(runtime_directory, session_id) {
        return SessionRemoval::ReplacingItsImage;
    }
    let Ok(current_endpoint_file) = load_session_endpoint_file(runtime_directory, session_id)
    else {
        return SessionRemoval::Rebound;
    };
    if current_endpoint_file.as_ref() != expected_endpoint_file {
        return SessionRemoval::Rebound;
    }
    if let Some(current_endpoint_file) = &current_endpoint_file {
        let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
        if is_refusal_from_live_session(&endpoint_path, current_endpoint_file.process_id) {
            return SessionRemoval::ProcessStillRunning {
                process_id: current_endpoint_file.process_id,
            };
        }
    }
    remove_session_files(
        runtime_directory,
        config_directory,
        session_registry,
        session_id,
        current_endpoint_file.as_ref(),
    );
    SessionRemoval::Removed
}

/// Drop `session_id` from the list and remove every file it left, with no
/// check: its program file, its endpoint file, its resume file, and on Unix
/// its socket file in `runtime_directory`. Those four paths come from the id:
/// an entry that was never in the list is cleaned the same way.
///
/// What the session advertised in the shared directory goes through
/// [`remove_shared_session_advertisement`], from the socket address
/// `advertised_endpoint_file` names. A session another local user started has
/// no endpoint file here: nothing of that user's is removed.
///
/// `config_directory` holds the `koshi.kdl` that names the shared directory a
/// removed session's Windows marker sits in.
fn remove_session_files(
    runtime_directory: &Path,
    config_directory: Option<&Path>,
    session_registry: &mut SessionRegistry,
    session_id: SessionId,
    advertised_endpoint_file: Option<&EndpointFile>,
) {
    session_registry.remove(&session_id);
    let _ = std::fs::remove_file(ServerProgramFile::resolve_session_program_file_path(
        runtime_directory,
        session_id,
    ));
    let _ = std::fs::remove_file(EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ));
    let _ = std::fs::remove_file(resolve_resume_file_path(runtime_directory, session_id));
    remove_socket_file(&compute_socket_address(runtime_directory, session_id));
    if let Some(advertised_endpoint_file) = advertised_endpoint_file {
        remove_shared_session_advertisement(
            session_id,
            &advertised_endpoint_file.socket_address,
            config_directory,
        );
    }
}

/// The Unix process id for `process_id`, or `None` for `0` and for a value
/// past the range of `pid_t`. Example: `5000` gives `Some(5000)`, and
/// `4294967295` gives `None`.
#[cfg(unix)]
fn convert_to_unix_process_id(process_id: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(process_id)
        .ok()
        .filter(|unix_process_id| *unix_process_id > 0)
}

/// Remove what the session `session_id` of this user's advertised in the
/// shared directory, given the socket address its endpoint file named.
///
/// On Unix, the socket file at `advertised_socket_address` is removed when its
/// file name is `session-<uuid>.sock` for `session_id`, such as
/// `/tmp/koshi/1000/session-<uuid>.sock`. A file of any other name is left
/// alone. On Windows the address is a pipe name, and the marker
/// `session-<uuid>` for `session_id` in the shared directory `koshi.kdl` in
/// `config_directory` names is removed, whether or not `allow-other-users` is
/// on.
fn remove_shared_session_advertisement(
    session_id: SessionId,
    advertised_socket_address: &str,
    config_directory: Option<&Path>,
) {
    #[cfg(unix)]
    {
        let _ = config_directory;
        let advertised_socket_file_name = format!("{session_id}.sock");
        if Path::new(advertised_socket_address)
            .file_name()
            .is_some_and(|socket_file_name| {
                socket_file_name == advertised_socket_file_name.as_str()
            })
        {
            remove_socket_file(advertised_socket_address);
        }
    }
    #[cfg(windows)]
    {
        let _ = advertised_socket_address;
        if let Some(shared_sessions_directory) =
            koshi_link::config::resolve_shared_sessions_directory(
                &koshi_link::config::load_current_server_config(config_directory),
            )
        {
            remove_advertisement_marker(&resolve_advertisement_marker_path(
                &shared_sessions_directory,
                session_id,
            ));
        }
    }
}

/// Remove every resume file in `runtime_directory` that has no endpoint file
/// beside it and is older than
/// [`RESTART_WINDOW_DURATION`](koshi_ipc::endpoint::RESTART_WINDOW_DURATION).
///
/// A swap that never reached its new image leaves the file behind with no
/// endpoint file beside it: the walk over endpoint files does not see it. That
/// happens when the new image is killed before it reads the file, and when the
/// machine loses power mid-swap. A new image that starts removes the file on
/// every way out.
///
/// A file younger than the window belongs to a swap that is still in flight. A
/// file with an endpoint file beside it, readable or not, and a file whose
/// endpoint file cannot be looked up, are left in place. A runtime directory
/// that cannot be read removes nothing.
fn remove_orphan_resume_files(runtime_directory: &Path) {
    let Ok(resumable_session_ids) = ipc_client::list_sessions_with_resume_files(runtime_directory)
    else {
        return;
    };
    for session_id in resumable_session_ids {
        let endpoint_file_lookup = std::fs::symlink_metadata(
            EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id),
        );
        let has_endpoint_file = !matches!(
            &endpoint_file_lookup,
            Err(lookup_error) if lookup_error.kind() == std::io::ErrorKind::NotFound
        );
        if has_endpoint_file || is_replacing_its_image(runtime_directory, session_id) {
            continue;
        }
        let _ = std::fs::remove_file(resolve_resume_file_path(runtime_directory, session_id));
    }
}

/// Build the command that starts one session server: its identity on the
/// command line, its output piped back for the ready report, and the
/// directory its first shell opens in.
///
/// `is_other_user_access_allowed` `Some(true)` adds [`ALLOW_OTHER_USERS_FLAG`]; any other
/// value leaves the session to its own `koshi.kdl`.
///
/// On Windows the server runs with the `CREATE_NO_WINDOW` creation flag: its
/// console has no window on screen.
fn build_session_server_command(
    runtime_directory: &Path,
    session_id: SessionId,
    session_name: &str,
    profile: Option<&str>,
    working_directory: Option<&Path>,
    is_other_user_access_allowed: Option<bool>,
) -> std::io::Result<std::process::Command> {
    let mut session_server_command =
        std::process::Command::new(koshi_host::program_path::resolve_program_path()?);
    session_server_command
        .arg(SESSION_SERVER_SUBCOMMAND)
        .arg(session_id.to_string())
        .arg(session_name)
        .arg(RUNTIME_DIRECTORY_FLAG)
        .arg(runtime_directory);
    if let Some(profile) = profile {
        session_server_command.arg(PROFILE_FLAG).arg(profile);
    }
    if is_other_user_access_allowed == Some(true) {
        session_server_command.arg(ALLOW_OTHER_USERS_FLAG);
    }
    if let Some(working_directory) = working_directory {
        session_server_command.current_dir(working_directory);
    }
    session_server_command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        session_server_command.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(session_server_command)
}

/// The line a freshly spawned session server printed, or the refusal to answer
/// with.
///
/// `None` is a session server that printed nothing readable before the wait
/// ran out. A report naming another control-plane protocol version is refused
/// with a sentence naming both versions and ending `run: koshi
/// restart-servers`.
fn validate_session_server_ready(
    ready_report: Option<SessionServerReady>,
) -> Result<SessionServerReady, String> {
    let Some(ready_report) = ready_report else {
        return Err("the session did not report a bound socket".to_string());
    };
    if ready_report.protocol_version != ROUTER_PROTOCOL_VERSION {
        return Err(format!(
            "the koshi binary on disk speaks control-plane protocol version {} and this running \
             router speaks {ROUTER_PROTOCOL_VERSION}, so they are different builds; run: koshi \
             restart-servers",
            ready_report.protocol_version
        ));
    }
    Ok(ready_report)
}

/// Watch one session server until it exits, then report the exit as
/// [`RouterEvent::ChildExited`].
///
/// Returns whether the thread started. A session whose thread could not start
/// has no exit watcher: [`start_unwatched_session_probes`] probes it, and on
/// Unix [`reap_unwaited_children`] reaps its process once it has exited.
fn start_session_reaper_thread(
    mut child_process: Child,
    session_id: SessionId,
    router_events_sender: Sender<RouterEvent>,
) -> bool {
    std::thread::Builder::new()
        .name("koshi-router-child".to_string())
        .spawn(move || {
            let _ = child_process.wait();
            let _ = router_events_sender.send(RouterEvent::ChildExited(session_id));
        })
        .is_ok()
}

/// Wait on every child in `child_process_ids`, each on a thread of its own
/// that reaps the child once it has exited and then sends
/// [`RouterEvent::ChildProcessReaped`] with its process id.
///
/// `child_process_ids` are the children an earlier image of this process
/// started, as [`process::list_child_process_ids`] lists them before this
/// image starts any. A child whose thread starts goes into
/// [`RouterSessions::inherited_child_process_ids`]. A child whose thread
/// cannot start goes into [`RouterSessions::unwaited_child_process_ids`]. An
/// id that [`convert_to_unix_process_id`] gives `None` for, such as `0`, is
/// skipped.
///
/// Before → after: the earlier image started a session server at process
/// `5000`, and that session ended while this image started, after it removed
/// its endpoint file → process `5000` is reaped once it exits, and no zombie
/// stays.
#[cfg(unix)]
fn adopt_inherited_children(
    child_process_ids: Vec<u32>,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
) {
    for child_process_id in child_process_ids {
        let Some(unix_process_id) = convert_to_unix_process_id(child_process_id) else {
            continue;
        };
        let exit_events_sender = router_events_sender.clone();
        let waiter_spawn_result = std::thread::Builder::new()
            .name("koshi-router-inherited-child".to_string())
            .spawn(move || {
                process::wait_for_child_exit(unix_process_id);
                let _ = exit_events_sender.send(RouterEvent::ChildProcessReaped {
                    process_id: child_process_id,
                });
            });
        match waiter_spawn_result {
            Ok(_) => {
                router_sessions
                    .inherited_child_process_ids
                    .insert(child_process_id);
            }
            Err(thread_spawn_error) => {
                tracing::warn!(
                    %thread_spawn_error,
                    process_id = child_process_id,
                    "no thread waits on an inherited child; it is reaped at a liveness check after it exits"
                );
                router_sessions
                    .unwaited_child_process_ids
                    .insert(child_process_id);
            }
        }
    }
}

/// Reap every exited child in `unwaited_child_process_ids` through `waitpid`
/// with `WNOHANG`. A child that still runs stays in the set. A child that is
/// reaped, an id that names no child of this process (`ECHILD`), and an id
/// that [`convert_to_unix_process_id`] gives `None` for leave the set. A
/// `waitpid` that a signal interrupts is made again.
///
/// # Errors
/// A [`ChildReapError`] for the first `waitpid` that fails with any other
/// error. That child, and every child not yet checked, stay in the set.
///
/// Before → after: `{5000, 5001}`, where `5000` has exited and `5001` runs →
/// `5000` is reaped, and the set is `{5001}`.
#[cfg(unix)]
fn reap_unwaited_children(
    unwaited_child_process_ids: &mut BTreeSet<u32>,
) -> Result<(), ChildReapError> {
    let mut child_reap_error = None;
    unwaited_child_process_ids.retain(|child_process_id| {
        if child_reap_error.is_some() {
            return true;
        }
        let Some(unix_process_id) = convert_to_unix_process_id(*child_process_id) else {
            return false;
        };
        loop {
            let mut process_wait_status = 0;
            // SAFETY: `waitpid` writes only to `process_wait_status`, which
            // lives for the call. `WNOHANG` returns at once.
            let waited_process_id =
                unsafe { libc::waitpid(unix_process_id, &mut process_wait_status, libc::WNOHANG) };
            if waited_process_id == 0 {
                return true;
            }
            if waited_process_id > 0 {
                return false;
            }
            let wait_error_code = std::io::Error::last_os_error()
                .raw_os_error()
                .expect("an error from `last_os_error` carries its OS error code");
            match wait_error_code {
                libc::EINTR => continue,
                libc::ECHILD => return false,
                _ => {
                    child_reap_error = Some(ChildReapError {
                        process_id: *child_process_id,
                        wait_error_code,
                    });
                    return true;
                }
            }
        }
    });
    child_reap_error.map_or(Ok(()), Err)
}

/// Read the one ready line a session server prints. End of stream or a line
/// that is not a readable report is `None`.
fn read_session_server_ready_line(
    session_server_stdout: ChildStdout,
) -> Option<SessionServerReady> {
    let mut ready_line = String::new();
    BufReader::new(session_server_stdout)
        .read_line(&mut ready_line)
        .ok()?;
    serde_json::from_str(&ready_line).ok()
}

/// Kill a child that never became a session, and wait for it to exit.
fn terminate_child_process(child_process: &mut Child) {
    let _ = child_process.kill();
    let _ = child_process.wait();
}

/// A refusal carrying `refusal_message`, under
/// [`IpcErrorCode::RequestFailed`].
///
/// Every refusal of a request the router read takes this code except a session
/// it does not have, which goes through [`build_session_not_found_result`].
fn build_refused_result(refusal_message: String) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::RequestFailed,
        message: refusal_message,
    })
}

/// A refusal for a selector naming a session the router does not have, under
/// [`IpcErrorCode::NotFound`].
///
/// The message names the selector. A [`SessionSelector::SessionId`] gives
/// `no session session-<uuid> is running`, and a [`SessionSelector::SessionName`] of
/// `quiet-lake` gives ``no session named `quiet-lake` is running``.
fn build_session_not_found_result(session_selector: &SessionSelector) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::NotFound,
        message: match session_selector {
            SessionSelector::SessionId(session_id) => format!("no session {session_id} is running"),
            SessionSelector::SessionName(session_name) => {
                format!("no session named `{session_name}` is running")
            }
        },
    })
}

/// A refusal for a name that several sessions other local users started
/// carry, through [`build_refused_result`]. The message lists their ids in
/// the order given: `2 sessions other local users started carry this name:
/// session-<uuid>, session-<uuid>; attach by session id`.
fn build_ambiguous_name_result(session_ids: &[SessionId]) -> RouterResult {
    let session_id_list = session_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>()
        .join(", ");
    build_refused_result(format!(
        "{} sessions other local users started carry this name: {session_id_list}; attach by \
         session id",
        session_ids.len()
    ))
}

/// A refusal for a selector naming a session that is still running and did
/// not answer, through [`build_refused_result`].
///
/// The message names the selector and `unanswered_session_reason`. A
/// [`SessionSelector::SessionId`] gives `session session-<uuid> is running but
/// did not answer: it is restarting`, and a [`SessionSelector::SessionName`]
/// of `quiet-lake` gives ``session named `quiet-lake` is running but did not
/// answer: it is restarting``.
fn build_unanswered_session_result(
    session_selector: &SessionSelector,
    unanswered_session_reason: impl fmt::Display,
) -> RouterResult {
    build_refused_result(match session_selector {
        SessionSelector::SessionId(session_id) => {
            format!(
                "session {session_id} is running but did not answer: {unanswered_session_reason}"
            )
        }
        SessionSelector::SessionName(session_name) => format!(
            "session named `{session_name}` is running but did not answer: \
             {unanswered_session_reason}"
        ),
    })
}
