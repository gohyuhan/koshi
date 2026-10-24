//! The TLS port this machine serves remote clients on.
//!
//! One port per machine, opened by the router. A remote client opens a TLS
//! stream, presents the secret from a grant, and then either lists the
//! sessions that secret reaches or attaches to one. The router keeps the
//! connection: it opens its own local connection to the session server,
//! presents that session's endpoint token on the client's behalf, and carries
//! the bytes both ways without reading them.
//!
//! The dispatcher answers three questions per connection: what a secret
//! reaches, which sessions a scope reaches, and where one named session
//! listens. Carrying a connection's traffic never reaches the dispatcher.
//!
//! An admitted secret registers the connection with the router, and it stays
//! registered until this listener reports it ended. A revoke shuts a
//! registered connection's socket, attached or not. The router holds at most
//! [`MAX_LIVE_REMOTE_CONNECTION_COUNT`](crate::router::MAX_LIVE_REMOTE_CONNECTION_COUNT)
//! registrations and refuses the connections that arrive over that count.
//!
//! The TLS handshake, the frame the caller opens with, and the refusal naming
//! both version ranges finish inside `ADMISSION_WINDOW_DURATION`, counted from
//! the moment the connection's thread starts. Each single read and write inside
//! them is given the time left on that deadline when it starts. Every other
//! refusal replaces that deadline with `REFUSAL_WINDOW_DURATION`. Once the
//! secret is admitted both halves lose their deadline. Each socket read and
//! write then waits at most `ADMITTED_FRAME_TIMEOUT_DURATION`, the Welcome
//! included. Once a bridge carries the connection, each socket write, and each
//! socket read after the client's first byte through the bridge, waits at most
//! the Attach's `link_timeout_ms`. A socket read before that byte waits at most
//! the larger of the two. An Attach that names no `link_timeout_ms` gives the
//! bridge no limit. A read or write that waits longer ends the connection, and
//! its registration with it. Example: with a `link_timeout_ms` of `30000`, a
//! paste that takes two minutes to arrive keeps the connection while each
//! socket read gets a byte inside 30 seconds.
//!
//! Every refusal closes the connection. An admitted caller whose attach
//! arrives while the router is about to restart into a new build is refused
//! with [`ROUTER_RESTARTING_MESSAGE`](koshi_ipc::router::ROUTER_RESTARTING_MESSAGE),
//! whatever its selector names. An admitted caller whose attach names a
//! session its secret reaches, and that session cannot be opened, is refused
//! with [`SESSION_NOT_ANSWERING_MESSAGE`](koshi_ipc::remote_wire::SESSION_NOT_ANSWERING_MESSAGE).
//! A caller that opens with the Hello of koshi 0.3.0 or 0.4.0 gets the
//! refusal naming both version ranges, and no secret is read from it. Every
//! other refusal but the one naming both version ranges is
//! [`REMOTE_REFUSED`](koshi_ipc::remote_wire::REMOTE_REFUSED). The refusal
//! of a secret also carries the remote protocol version settled from the
//! Hello. A wrong secret, a revoked secret, and an expired secret produce the
//! same bytes and the same work. A session that does not exist, a session the
//! secret holds no grant for, and a session another local user started also
//! produce the same bytes and the same work: no caller-supplied name
//! reaches a socket connect, a wait, or a file until the admitted scope has
//! been proven to cover it. Order is `admit_remote_token` → the wait for every
//! unlisted session of this user's to describe itself, whatever the selector
//! names → `resolve_session_selector`, which must name a session of this
//! user's → `is_allowed_for_session` → open.
//!
//! This listener carries the three remote frames and then one session server's
//! own bytes. No path from it reaches the router's control plane, so
//! `koshi share` is unreachable over a remote connection. A client counts as
//! remote when this listener accepted it: the router marks the Hello it sends
//! the session server on that client's behalf. A caller can add that mark to
//! itself and cannot take it off.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection};
use serde::Deserialize;

use koshi_core::constant::MIN_LINK_TIMEOUT_MS;
use koshi_core::ids::SessionId;
use koshi_ipc::endpoint::{is_replacing_its_image, EndpointFile};
use koshi_ipc::protocol::{
    compute_agreed_protocol_version, ConnectionToken, IpcRequest, IpcRequestKind,
};
use koshi_ipc::remote_state::CertificateFile;
use koshi_ipc::remote_tokens::TokenScope;
use koshi_ipc::remote_wire::{
    format_version_refusal, RemoteClientFrame, RemoteServerFrame, RemoteSessionRow,
    MIN_REMOTE_PROTOCOL_VERSION, REMOTE_HELLO_MAX_BYTE_COUNT, REMOTE_PROTOCOL_VERSION,
    REMOTE_REFUSED, SESSION_NOT_ANSWERING_MESSAGE,
};
use koshi_ipc::router::{SessionSelector, ROUTER_RESTARTING_MESSAGE};
use koshi_ipc::tls::{self, TlsReader, TlsWriter};
use koshi_ipc::transport::{
    Connection, Deadlined, RawReader, RawWriter, ReadCloser, CONNECT_WAIT_DURATION,
    MAX_FRAME_BYTE_COUNT,
};
use koshi_link::ipc_client::RESTART_POLL_INTERVAL_DURATION;
use koshi_observability::repeated_warning::WarningRateLimiter;
use koshi_runtime::executable_watch::ExecutableWatch;

use crate::router::{check_router_executable_file, RouterEvent};

/// How long the connection's thread spends on the TLS handshake, on reading
/// the frame the caller opens with, and on writing every refusal it answers
/// before admission, counted from the moment that thread starts: 10 s. A
/// caller that is not admitted holds its thread and its admission place for no
/// longer than this.
const ADMISSION_WINDOW_DURATION: Duration = Duration::from_secs(10);

/// How long one address's connection attempts are counted over.
const RATE_WINDOW_DURATION: Duration = Duration::from_secs(60);

/// How many connections from one address may be inside the admission window,
/// or have left it without being admitted, inside [`RATE_WINDOW_DURATION`]
/// before the rest are dropped. A connection whose secret is admitted stops
/// counting at its admission.
const MAX_UNADMITTED_ATTEMPT_COUNT: u32 = 10;

/// How many peer addresses the attempt table counts at once.
const MAX_RATE_TABLE_ENTRY_COUNT: usize = 1024;

/// How many connections may be inside the admission window at once, across
/// every address.
///
/// A connection is counted from the moment it is accepted until its secret is
/// admitted or it goes away. One arriving over this count is closed without a
/// handshake. An admitted connection is not counted here; it counts against
/// [`MAX_LIVE_REMOTE_CONNECTION_COUNT`](crate::router::MAX_LIVE_REMOTE_CONNECTION_COUNT) instead.
const MAX_ADMISSION_COUNT: usize = 64;

/// How long the accept loop pauses after a failed accept before trying again.
const ACCEPT_RETRY_DELAY_DURATION: Duration = Duration::from_millis(100);

/// How long a refusal has to reach the caller it answers, counted from the
/// write. A refusal written before admission is cut short at the admission
/// deadline instead.
const REFUSAL_WINDOW_DURATION: Duration = Duration::from_secs(10);

/// How long one socket read or write on an admitted connection waits before a
/// bridge carries it: 20 s. A read that gets no byte, or a write that takes no
/// byte, for that long ends the connection. A socket read through a bridge
/// before the client's first byte there waits at least this long.
const ADMITTED_FRAME_TIMEOUT_DURATION: Duration = Duration::from_secs(20);

/// When a refusal written inside the admission window must be on the socket:
/// [`REFUSAL_WINDOW_DURATION`] from now, or `admission_deadline`, whichever comes first.
fn compute_refusal_deadline(admission_deadline: Instant) -> Instant {
    admission_deadline.min(Instant::now() + REFUSAL_WINDOW_DURATION)
}

/// One question a connection thread puts to the router's dispatcher.
pub(crate) enum AdmissionAsk {
    /// What a presented secret reaches. A secret that reaches something
    /// registers this connection with the router.
    Admit {
        /// The secret the caller presented.
        connection_token: ConnectionToken,
        /// The connection's socket. A revoke shuts it.
        remote_connection_stream: TcpStream,
        /// Where the answer goes. `None` refuses the connection.
        response_sender: Sender<Option<RemoteConnectionAdmission>>,
    },
    /// List the sessions an admitted scope reaches.
    ListRows {
        /// How far the admitting grant reaches.
        scope: TokenScope,
        /// Where the answer goes.
        response_sender: Sender<Vec<RemoteSessionRow>>,
    },
    /// Which session a selector names, when this connection still stands and
    /// the admitted scope covers it.
    Locate {
        /// How far the admitting grant reaches.
        scope: TokenScope,
        /// The number this connection is registered under.
        remote_connection_id: u64,
        /// The session the caller named.
        session_selector: SessionSelector,
        /// Where the answer goes: the session to attach to, or why the attach
        /// is refused.
        response_sender: Sender<Result<LocatedSession, LocateRefusal>>,
    },
    /// Remove one admitted connection that has ended from the router's list.
    RemoveConnection {
        /// The number that connection was registered under.
        remote_connection_id: u64,
    },
}

/// The session a remote attach reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocatedSession {
    /// The directory that holds the session's endpoint file and resume file.
    pub(crate) runtime_directory: PathBuf,
    /// The session the attach named.
    pub(crate) session_id: SessionId,
}

/// Why the dispatcher refused a remote attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocateRefusal {
    /// The connection was dropped by a revoke, the selector names no session
    /// of this user's, or the scope does not cover that session. The caller
    /// gets [`REMOTE_REFUSED`].
    NotReached,
    /// The router has decided to restart into a new build and attaches
    /// nothing until it has. The caller gets
    /// [`ROUTER_RESTARTING_MESSAGE`].
    RouterRestarting,
}

/// What a presented secret reached.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RemoteConnectionAdmission {
    /// How far the grant behind that secret reaches.
    pub scope: TokenScope,
    /// The number this connection is registered under, named again when it
    /// attaches and when it ends.
    pub remote_connection_id: u64,
}

/// A TLS port this machine holds and is not yet serving on.
///
/// Dropping this without calling [`BoundRemoteListener::start_serving`] gives the port back.
pub(crate) struct BoundRemoteListener {
    /// Sends the accept loop what it needs to start: the sender of the
    /// router's dispatcher and the router's watch of its program file.
    /// Dropping this without sending ends the waiting thread, which gives the
    /// port back.
    dispatcher_sender: Sender<(Sender<RouterEvent>, Arc<ExecutableWatch>)>,
}

/// Take the TLS port at `remote_listen_address`, presenting
/// `certificate_file`, without serving on it yet.
///
/// Builds the TLS configuration, binds `remote_listen_address`, and starts the
/// accept thread. That thread holds the port and accepts nobody until
/// [`BoundRemoteListener::start_serving`] sends it the dispatcher's events
/// sender. Dropping the [`BoundRemoteListener`] first ends the thread and
/// releases the port.
///
/// # Errors
/// The certificate that could not be turned into a TLS configuration, or the
/// address that could not be bound.
pub(crate) fn bind_remote_listener(
    remote_listen_address: SocketAddr,
    certificate_file: &CertificateFile,
) -> io::Result<BoundRemoteListener> {
    let tls_config = Arc::new(build_remote_server_tls_config(certificate_file)?);
    let listener = TcpListener::bind(remote_listen_address)?;
    let (dispatcher_sender, dispatcher_receiver) =
        mpsc::channel::<(Sender<RouterEvent>, Arc<ExecutableWatch>)>();
    std::thread::Builder::new()
        .name("koshi-remote-accept".to_string())
        .spawn(move || {
            let Ok((dispatcher_events_sender, executable_watch)) = dispatcher_receiver.recv()
            else {
                return;
            };
            run_remote_accept_loop(
                &listener,
                &tls_config,
                &dispatcher_events_sender,
                &executable_watch,
            );
        })?;
    Ok(BoundRemoteListener { dispatcher_sender })
}

impl BoundRemoteListener {
    /// Start serving on this port. The thread [`bind_remote_listener`] started
    /// begins accepting connections and gives each its own thread;
    /// `dispatcher_events_sender` carries those threads' questions to the
    /// router's dispatcher, and each connection served runs a check of
    /// `executable_watch`, as [`check_router_executable_file`] states.
    ///
    /// Cannot fail.
    pub(crate) fn start_serving(
        self,
        dispatcher_events_sender: Sender<RouterEvent>,
        executable_watch: Arc<ExecutableWatch>,
    ) {
        let _ = self
            .dispatcher_sender
            .send((dispatcher_events_sender, executable_watch));
    }
}

/// The TLS configuration this machine serves with: TLS 1.3 only,
/// `certificate_file`'s certificate and private key, and no client certificate
/// asked for.
fn build_remote_server_tls_config(certificate_file: &CertificateFile) -> io::Result<ServerConfig> {
    let certificate_chain = vec![CertificateDer::from(certificate_file.cert_der.clone())];
    let private_key =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certificate_file.key_der.clone()));
    ServerConfig::builder_with_provider(koshi_ipc::tls::build_crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("aws-lc-rs supports TLS 1.3")
        .with_no_client_auth()
        .with_single_cert(certificate_chain, private_key)
        .map_err(|certificate_error| io::Error::other(certificate_error.to_string()))
}

/// Accept connections and give each its own thread, dropping the ones from an
/// address that already counts [`MAX_UNADMITTED_ATTEMPT_COUNT`] connections
/// not admitted inside [`RATE_WINDOW_DURATION`], and the ones that arrive
/// while [`MAX_ADMISSION_COUNT`] connections are already waiting to present a
/// secret. A connection dropped for a full admission window does not count
/// against its address. A failed accept is reported at most once inside
/// [`LOG_WINDOW_DURATION`](koshi_observability::repeated_warning::LOG_WINDOW_DURATION),
/// waits [`ACCEPT_RETRY_DELAY_DURATION`], and retries.
/// Each connection given a thread runs a check of `executable_watch` first.
fn run_remote_accept_loop(
    listener: &TcpListener,
    tls_config: &Arc<ServerConfig>,
    dispatcher_events_sender: &Sender<RouterEvent>,
    executable_watch: &Arc<ExecutableWatch>,
) {
    let peer_address_rate_table = Arc::new(Mutex::new(PeerAddressRateTable::new()));
    let admission_count = Arc::new(AtomicUsize::new(0));
    let mut full_admission_warning = WarningRateLimiter::new();
    let mut accept_failure_warning = WarningRateLimiter::new();
    loop {
        let (tcp_stream, peer_address) = match listener.accept() {
            Ok(accepted_connection) => accepted_connection,
            Err(accept_error) => {
                if accept_failure_warning.record_warning_if_due(Instant::now()) {
                    tracing::warn!(
                        %accept_error,
                        "the remote port could not accept a connection; \
                         retrying every {ACCEPT_RETRY_DELAY_DURATION:?}"
                    );
                }
                std::thread::sleep(ACCEPT_RETRY_DELAY_DURATION);
                continue;
            }
        };
        let current_time = Instant::now();
        let Some(admission_slot) = AdmissionSlot::enter_admission(&admission_count) else {
            if full_admission_warning.record_warning_if_due(current_time) {
                tracing::warn!(
                    "{MAX_ADMISSION_COUNT} remote connections are waiting to present a secret; \
                     closing the ones that arrive until some of them finish"
                );
            }
            drop(tcp_stream);
            continue;
        };
        let peer_ip_address = peer_address.ip();
        let attempt_decision = peer_address_rate_table
            .lock()
            .expect("peer address rate table")
            .decide_attempt(peer_ip_address, current_time);
        let window_started_at = match attempt_decision {
            PeerAddressAttemptDecision::Serve { window_started_at } => window_started_at,
            PeerAddressAttemptDecision::DropAndSay => {
                tracing::warn!(
                    %peer_ip_address,
                    "{MAX_UNADMITTED_ATTEMPT_COUNT} remote connections from {peer_ip_address} were \
                     not admitted within {RATE_WINDOW_DURATION:?}; dropping the rest until the window \
                     passes"
                );
                drop(tcp_stream);
                continue;
            }
            PeerAddressAttemptDecision::DropInSilence => {
                drop(tcp_stream);
                continue;
            }
        };
        let counted_attempt = CountedAttempt {
            peer_address_rate_table: Arc::clone(&peer_address_rate_table),
            peer_ip_address,
            window_started_at,
        };
        check_router_executable_file(executable_watch, dispatcher_events_sender);
        let tls_config = Arc::clone(tls_config);
        let dispatcher_events_sender = dispatcher_events_sender.clone();
        let _ = std::thread::Builder::new()
            .name("koshi-remote".to_string())
            .spawn(move || {
                serve_remote_connection(
                    tcp_stream,
                    &tls_config,
                    &dispatcher_events_sender,
                    admission_slot,
                    counted_attempt,
                )
            });
    }
}

/// One connection counted in its address's window of the
/// [`PeerAddressRateTable`]. It stays counted until
/// [`remove_attempt`](Self::remove_attempt) runs at its admission. A connection
/// that ends any other way stays counted until its window passes.
struct CountedAttempt {
    /// The table the connection is counted in.
    peer_address_rate_table: Arc<Mutex<PeerAddressRateTable>>,
    /// The address the connection came from.
    peer_ip_address: IpAddr,
    /// When the window the connection is counted in opened.
    window_started_at: Instant,
}

impl CountedAttempt {
    /// Take this connection out of its address's count, as
    /// [`PeerAddressRateTable::remove_attempt`] states.
    fn remove_attempt(self) {
        self.peer_address_rate_table
            .lock()
            .expect("peer address rate table")
            .remove_attempt(self.peer_ip_address, self.window_started_at);
    }
}

/// One connection inside the admission window, counted while it is there.
///
/// The count drops when this is dropped, whichever way the connection left:
/// admitted, refused, timed out, or hung up.
struct AdmissionSlot {
    /// The shared count of connections inside the window.
    admission_count: Arc<AtomicUsize>,
}

impl AdmissionSlot {
    /// Count one more connection, or `None` when [`MAX_ADMISSION_COUNT`] are
    /// already inside the window.
    fn enter_admission(admission_count: &Arc<AtomicUsize>) -> Option<AdmissionSlot> {
        let is_admission_slot_taken = admission_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |admission_count| {
                (admission_count < MAX_ADMISSION_COUNT).then_some(admission_count + 1)
            })
            .is_ok();
        if !is_admission_slot_taken {
            return None;
        }
        Some(AdmissionSlot {
            admission_count: Arc::clone(admission_count),
        })
    }
}

impl Drop for AdmissionSlot {
    fn drop(&mut self) {
        self.admission_count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What the rate table says to do with one connection attempt.
#[derive(Debug, PartialEq, Eq)]
enum PeerAddressAttemptDecision {
    /// Serve it: this address is inside its limit. The attempt is counted in
    /// the window that opened at `window_started_at`.
    Serve {
        /// When the window the attempt is counted in opened.
        window_started_at: Instant,
    },
    /// Drop it, and log that the address crossed its limit. This is the first
    /// attempt over the limit in this window.
    DropAndSay,
    /// Drop it without logging. This address crossed its limit earlier in the
    /// same window.
    DropInSilence,
}

/// What one address has done inside the window it opened.
struct PeerAddressRateWindow {
    /// How many connections from that address served since `window_started_at`
    /// are not admitted: the ones still inside the admission window and the
    /// ones that left it without being admitted.
    unadmitted_attempt_count: u32,
    /// Whether a dropped attempt from that address was logged inside this
    /// window.
    is_limit_reported: bool,
    /// When the first of them arrived.
    window_started_at: Instant,
}

/// How many connections each address has opened lately and not had admitted.
///
/// Bounded at [`MAX_RATE_TABLE_ENTRY_COUNT`]. Every check first drops the
/// addresses whose window has passed; a check that still finds the table full
/// drops the address whose window opened first.
struct PeerAddressRateTable {
    /// One window per address.
    window_by_peer_ip_address: HashMap<IpAddr, PeerAddressRateWindow>,
}

impl PeerAddressRateTable {
    /// An empty table.
    fn new() -> PeerAddressRateTable {
        PeerAddressRateTable {
            window_by_peer_ip_address: HashMap::new(),
        }
    }

    /// Say what to do with one connection from `peer_ip_address` at
    /// `current_time`, and count it when it is served.
    ///
    /// A connection is served while its address counts fewer than
    /// [`MAX_UNADMITTED_ATTEMPT_COUNT`] connections. A dropped connection is
    /// not counted. An address is logged once per window, on the first
    /// connection dropped in it; every other one dropped in that window is
    /// dropped in silence.
    ///
    /// Example — with [`MAX_UNADMITTED_ATTEMPT_COUNT`] at 10 and no
    /// connection admitted, attempts 1 to 10 from one address are
    /// [`PeerAddressAttemptDecision::Serve`], attempt 11 is
    /// [`PeerAddressAttemptDecision::DropAndSay`], and attempts 12 onward are
    /// [`PeerAddressAttemptDecision::DropInSilence`] until the window passes
    /// or [`remove_attempt`](Self::remove_attempt) takes one of the 10 out.
    fn decide_attempt(
        &mut self,
        peer_ip_address: IpAddr,
        current_time: Instant,
    ) -> PeerAddressAttemptDecision {
        self.window_by_peer_ip_address
            .retain(|_, peer_address_rate_window| {
                current_time.duration_since(peer_address_rate_window.window_started_at)
                    < RATE_WINDOW_DURATION
            });
        if self.window_by_peer_ip_address.len() >= MAX_RATE_TABLE_ENTRY_COUNT
            && !self
                .window_by_peer_ip_address
                .contains_key(&peer_ip_address)
        {
            let oldest_peer_ip_address = self
                .window_by_peer_ip_address
                .iter()
                .min_by_key(|(_, peer_address_rate_window)| {
                    peer_address_rate_window.window_started_at
                })
                .map(|(peer_ip_address, _)| *peer_ip_address);
            if let Some(oldest_peer_ip_address) = oldest_peer_ip_address {
                self.window_by_peer_ip_address
                    .remove(&oldest_peer_ip_address);
            }
        }
        let peer_address_rate_window = self
            .window_by_peer_ip_address
            .entry(peer_ip_address)
            .or_insert(PeerAddressRateWindow {
                unadmitted_attempt_count: 0,
                is_limit_reported: false,
                window_started_at: current_time,
            });
        if peer_address_rate_window.unadmitted_attempt_count < MAX_UNADMITTED_ATTEMPT_COUNT {
            peer_address_rate_window.unadmitted_attempt_count += 1;
            return PeerAddressAttemptDecision::Serve {
                window_started_at: peer_address_rate_window.window_started_at,
            };
        }
        if peer_address_rate_window.is_limit_reported {
            return PeerAddressAttemptDecision::DropInSilence;
        }
        peer_address_rate_window.is_limit_reported = true;
        PeerAddressAttemptDecision::DropAndSay
    }

    /// Take one served connection from `peer_ip_address` out of the count of
    /// the window that opened at `window_started_at`. Nothing changes when
    /// that address's window is gone or opened at another moment: the
    /// connection was counted in a window that has passed.
    fn remove_attempt(&mut self, peer_ip_address: IpAddr, window_started_at: Instant) {
        let Some(peer_address_rate_window) =
            self.window_by_peer_ip_address.get_mut(&peer_ip_address)
        else {
            return;
        };
        if peer_address_rate_window.window_started_at != window_started_at {
            return;
        }
        peer_address_rate_window.unadmitted_attempt_count = peer_address_rate_window
            .unadmitted_attempt_count
            .saturating_sub(1);
    }
}

/// What the frame a caller sends turned out to be.
#[derive(Debug, PartialEq, Eq)]
enum RemoteClientFrameRead {
    /// A readable frame.
    Frame(RemoteClientFrame),
    /// The Hello that koshi 0.3.0 and 0.4.0 open with, which speaks remote
    /// protocol version 1. Carries the remote protocol range it names.
    PreviousReleaseHello {
        minimum_remote_version: u32,
        maximum_remote_version: u32,
    },
    /// Bytes that are not a readable frame. The caller is refused.
    Unreadable,
    /// A length prefix past the cap, or a stream that ended or timed out.
    /// Nothing is written back.
    Closed,
}

/// The Hello that koshi 0.3.0 and 0.4.0 open with, read as far as the remote
/// protocol range it names. The fields it also carries are not read.
///
/// Example — `{"Hello":{"min_remote_version":1,"max_remote_version":1,
/// "min_protocol_version":2,"max_protocol_version":3,"token":"k7QxSecret"}}`
/// reads as the range 1 to 1.
#[derive(Deserialize)]
enum PreviousReleaseRemoteClientFrame {
    Hello {
        /// The lowest remote protocol version the client speaks.
        #[serde(rename = "min_remote_version")]
        minimum_remote_version: u32,
        /// The highest remote protocol version the client speaks.
        #[serde(rename = "max_remote_version")]
        maximum_remote_version: u32,
    },
}

/// Serve one remote connection: the TLS handshake, the secret, and then either
/// the sessions that secret reaches or a bridge to one of them.
///
/// The TLS handshake and the frame the caller opens with finish inside
/// [`ADMISSION_WINDOW_DURATION`], counted from the moment this thread starts. A
/// refusal written by [`send_refusal`] gets [`REFUSAL_WINDOW_DURATION`]
/// instead. Once the caller is admitted both halves lose their deadlines, and
/// each socket read and write waits at most [`ADMITTED_FRAME_TIMEOUT_DURATION`]
/// for a byte until a bridge carries the connection.
///
/// Admission registers the connection with the router, attached or not. The
/// registration is dropped when the connection finishes, whichever step it
/// finished at.
///
/// `admission_slot` holds this connection's place in the admission window and
/// is dropped the moment the secret is admitted. `counted_attempt` counts this
/// connection against its address, and is removed from that count the moment
/// the secret is admitted.
///
/// On Unix the thread blocks SIGPIPE on its own signal mask; a write to a peer
/// that hung up returns an error whatever the process-wide disposition is.
fn serve_remote_connection(
    mut tcp_stream: TcpStream,
    tls_config: &Arc<ServerConfig>,
    dispatcher_events_sender: &Sender<RouterEvent>,
    admission_slot: AdmissionSlot,
    counted_attempt: CountedAttempt,
) {
    #[cfg(unix)]
    crate::process::block_sigpipe_on_this_thread();

    let admission_deadline = Instant::now() + ADMISSION_WINDOW_DURATION;
    let Ok(control_stream) = tcp_stream.try_clone() else {
        return;
    };
    let Ok(server_connection) = ServerConnection::new(Arc::clone(tls_config)) else {
        return;
    };
    let mut tls_connection = rustls::Connection::Server(server_connection);
    if tls::run_tls_handshake(&mut tls_connection, &mut tcp_stream, admission_deadline).is_err() {
        return;
    }
    let Ok((mut reader, mut writer)) = tls::split_tls_stream(tls_connection, tcp_stream) else {
        return;
    };

    reader.set_deadline(Some(admission_deadline));
    writer.set_deadline(Some(admission_deadline));
    let (
        (minimum_remote_version, maximum_remote_version),
        session_protocol_versions,
        connection_token,
    ) = match read_client_frame(&mut reader, REMOTE_HELLO_MAX_BYTE_COUNT) {
        RemoteClientFrameRead::Frame(RemoteClientFrame::Hello {
            minimum_remote_version,
            maximum_remote_version,
            minimum_protocol_version,
            maximum_protocol_version,
            connection_token,
        }) => (
            (minimum_remote_version, maximum_remote_version),
            (minimum_protocol_version, maximum_protocol_version),
            connection_token,
        ),
        RemoteClientFrameRead::PreviousReleaseHello {
            minimum_remote_version,
            maximum_remote_version,
        } => {
            send_refusal_message(
                &mut writer,
                &format_version_refusal(minimum_remote_version, maximum_remote_version),
                None,
                admission_deadline,
            );
            return;
        }
        RemoteClientFrameRead::Frame(_) | RemoteClientFrameRead::Unreadable => {
            send_refusal_with_deadline(&mut writer, compute_refusal_deadline(admission_deadline));
            return;
        }
        RemoteClientFrameRead::Closed => return,
    };

    // The version is checked before the secret. The version refusal names both
    // version ranges.
    let Some(agreed_remote_protocol_version) = compute_agreed_protocol_version(
        minimum_remote_version,
        maximum_remote_version,
        MIN_REMOTE_PROTOCOL_VERSION,
        REMOTE_PROTOCOL_VERSION,
    ) else {
        send_refusal_message(
            &mut writer,
            &format_version_refusal(minimum_remote_version, maximum_remote_version),
            None,
            admission_deadline,
        );
        return;
    };

    let Ok(registered_stream) = control_stream.try_clone() else {
        return;
    };
    let admission_response = ask_router_dispatcher(dispatcher_events_sender, |response_sender| {
        AdmissionAsk::Admit {
            connection_token,
            remote_connection_stream: registered_stream,
            response_sender,
        }
    });
    let Some(Some(admitted_connection)) = admission_response else {
        send_refusal_message(
            &mut writer,
            REMOTE_REFUSED,
            Some(agreed_remote_protocol_version),
            compute_refusal_deadline(admission_deadline),
        );
        return;
    };

    // The caller leaves the admission window and stops counting against its
    // address.
    drop(admission_slot);
    counted_attempt.remove_attempt();

    // Both halves lose their deadlines. Each socket read and write then waits
    // at most ADMITTED_FRAME_TIMEOUT_DURATION for a byte.
    reader.set_deadline(None);
    writer.set_deadline(None);
    reader.set_idle_timeout(Some(ADMITTED_FRAME_TIMEOUT_DURATION));
    writer.set_idle_timeout(Some(ADMITTED_FRAME_TIMEOUT_DURATION));
    if send_remote_frame(
        &mut writer,
        &RemoteServerFrame::Welcome {
            remote_protocol_version: agreed_remote_protocol_version,
        },
    )
    .is_err()
    {
        report_remote_connection_ended(
            dispatcher_events_sender,
            admitted_connection.remote_connection_id,
        );
        return;
    }

    serve_admitted_remote_connection(
        reader,
        writer,
        control_stream,
        admitted_connection,
        session_protocol_versions,
        dispatcher_events_sender,
    );
}

/// Serve an admitted connection: list the sessions its secret reaches, as
/// often as it asks, attach to one when it asks for that, and report the
/// connection ended when no bridge took it over.
///
/// The Attach's `link_timeout_ms` times the bridge, as
/// [`bridge_remote_connection_to_session`] states.
///
/// `session_protocol_versions` is the session protocol range the client named
/// in its opening frame. [`bridge_remote_connection_to_session`] puts it in the
/// session-plane Hello it sends for this client.
fn serve_admitted_remote_connection(
    mut reader: TlsReader,
    mut writer: TlsWriter,
    control_stream: TcpStream,
    admitted_connection: RemoteConnectionAdmission,
    session_protocol_versions: (u32, u32),
    dispatcher_events_sender: &Sender<RouterEvent>,
) {
    let attached_session = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        dispatcher_events_sender,
    );
    let Some((located_session, link_timeout_ms)) = attached_session else {
        report_remote_connection_ended(
            dispatcher_events_sender,
            admitted_connection.remote_connection_id,
        );
        return;
    };
    bridge_remote_connection_to_session(
        reader,
        writer,
        control_stream,
        &located_session,
        admitted_connection.remote_connection_id,
        session_protocol_versions,
        link_timeout_ms.map(|link_timeout_ms| Duration::from_millis(u64::from(link_timeout_ms))),
        dispatcher_events_sender,
    );
}

/// Read the frames an admitted connection sends until it attaches or ends.
///
/// A list is answered and the next frame is read, so one connection may list
/// and then attach. `Some` is the session an admitted attach reached, with that
/// attach's `link_timeout_ms`; the bytes after that attach belong to that
/// session's server. `None` means the connection is finished: it hung up, a
/// read ran out of time, it sent something this loop does not serve, its attach
/// was refused, or the dispatcher is gone.
///
/// An attach naming a `link_timeout_ms` below [`MIN_LINK_TIMEOUT_MS`],
/// `1000`, is refused before the dispatcher is asked where its session is.
fn process_admitted_remote_frames(
    reader: &mut impl Read,
    writer: &mut (impl Write + Deadlined),
    admitted_connection: &RemoteConnectionAdmission,
    dispatcher_events_sender: &Sender<RouterEvent>,
) -> Option<(LocatedSession, Option<u32>)> {
    loop {
        let remote_client_frame = match read_client_frame(reader, MAX_FRAME_BYTE_COUNT) {
            RemoteClientFrameRead::Frame(remote_client_frame) => remote_client_frame,
            RemoteClientFrameRead::PreviousReleaseHello { .. }
            | RemoteClientFrameRead::Unreadable => {
                send_refusal(writer);
                return None;
            }
            RemoteClientFrameRead::Closed => return None,
        };
        match remote_client_frame {
            RemoteClientFrame::List => {
                let admitted_token_scope = admitted_connection.scope.clone();
                let remote_session_rows =
                    ask_router_dispatcher(dispatcher_events_sender, |response_sender| {
                        AdmissionAsk::ListRows {
                            scope: admitted_token_scope,
                            response_sender,
                        }
                    })?;
                if send_remote_frame(
                    writer,
                    &RemoteServerFrame::Sessions {
                        session_rows: remote_session_rows,
                    },
                )
                .is_err()
                {
                    return None;
                }
            }
            RemoteClientFrame::Attach {
                session_selector,
                link_timeout_ms,
            } => {
                if link_timeout_ms
                    .is_some_and(|link_timeout_ms| link_timeout_ms < MIN_LINK_TIMEOUT_MS)
                {
                    send_refusal(writer);
                    return None;
                }
                let admitted_token_scope = admitted_connection.scope.clone();
                let remote_connection_id = admitted_connection.remote_connection_id;
                let locate_answer =
                    ask_router_dispatcher(dispatcher_events_sender, |response_sender| {
                        AdmissionAsk::Locate {
                            scope: admitted_token_scope,
                            remote_connection_id,
                            session_selector,
                            response_sender,
                        }
                    })?;
                return match locate_answer {
                    Ok(located_session) => Some((located_session, link_timeout_ms)),
                    Err(LocateRefusal::NotReached) => {
                        send_refusal(writer);
                        None
                    }
                    Err(LocateRefusal::RouterRestarting) => {
                        send_refusal_message(
                            writer,
                            ROUTER_RESTARTING_MESSAGE,
                            None,
                            Instant::now() + REFUSAL_WINDOW_DURATION,
                        );
                        None
                    }
                };
            }
            RemoteClientFrame::Hello { .. } => {
                send_refusal(writer);
                return None;
            }
        }
    }
}

/// The Hello the router sends a session server for a caller this listener
/// accepted: `connection_token` from that session's endpoint file, the
/// caller's own version range in `session_protocol_versions` as
/// `(minimum, maximum)`, and `is_remote` set to `true`.
///
/// This is the only place `is_remote` is set.
fn build_bridged_hello(
    connection_token: ConnectionToken,
    session_protocol_versions: (u32, u32),
) -> IpcRequest {
    let (minimum_protocol_version, maximum_protocol_version) = session_protocol_versions;
    IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            minimum_protocol_version,
            maximum_protocol_version,
            connection_token,
            is_remote: true,
        },
    }
}

/// Open the local connection to the session advertised at
/// `session_endpoint_path` and send it the Hello carrying that session's
/// endpoint token and `session_protocol_versions`.
///
/// Hands back the connection's two raw halves and the handle that closes its
/// read direction.
///
/// `None` when the endpoint file cannot be read, its socket cannot be reached
/// or another OS user serves it, the read direction cannot be made closable,
/// or the Hello cannot be sent.
fn open_local_session_bridge(
    session_endpoint_path: &Path,
    session_protocol_versions: (u32, u32),
) -> Option<(RawReader, RawWriter, ReadCloser)> {
    let session_endpoint = EndpointFile::load_from_path(session_endpoint_path).ok()?;
    let mut session_connection = Connection::connect_to_own_server_within(
        &session_endpoint.socket_address,
        CONNECT_WAIT_DURATION,
    )
    .ok()?;
    let session_read_closer = session_connection.create_read_closer().ok()?;
    session_connection
        .send(&build_bridged_hello(
            session_endpoint.connection_token,
            session_protocol_versions,
        ))
        .ok()?;
    let (session_reader, session_writer) = session_connection.split_raw();
    Some((session_reader, session_writer, session_read_closer))
}

/// Open the local connection to the session `located_session` names, as
/// [`open_local_session_bridge`] opens it. While an open fails and
/// [`is_replacing_its_image`] accepts the session, the open runs again every
/// [`RESTART_POLL_INTERVAL_DURATION`]. Once it no longer accepts the session,
/// one last open runs.
///
/// Example: a session that withdrew its endpoint file to start its new image
/// is opened once the new image advertises its own endpoint file, about one
/// second after the withdrawal.
///
/// `None` when the last open fails.
fn open_local_session_bridge_across_restart(
    located_session: &LocatedSession,
    session_protocol_versions: (u32, u32),
) -> Option<(RawReader, RawWriter, ReadCloser)> {
    let session_endpoint_path = EndpointFile::resolve_endpoint_file_path(
        &located_session.runtime_directory,
        located_session.session_id,
    );
    loop {
        if let Some(session_bridge) =
            open_local_session_bridge(&session_endpoint_path, session_protocol_versions)
        {
            return Some(session_bridge);
        }
        if !is_replacing_its_image(
            &located_session.runtime_directory,
            located_session.session_id,
        ) {
            return open_local_session_bridge(&session_endpoint_path, session_protocol_versions);
        }
        std::thread::sleep(RESTART_POLL_INTERVAL_DURATION);
    }
}

/// Open the local connection to the session `located_session` names for an
/// admitted client, as [`open_local_session_bridge_across_restart`] opens it,
/// and carry the bytes both ways.
///
/// The router sends the session-plane Hello carrying the session's endpoint
/// token and the client's version range. The session server's answer to that
/// Hello, and everything after it, travels back through the bridge unread. A
/// session that cannot be opened is refused with
/// [`SESSION_NOT_ANSWERING_MESSAGE`], and the connection is reported ended
/// with nothing bridged.
///
/// `link_timeout` is the Attach's `link_timeout_ms`. Each socket write to the
/// client waits at most `link_timeout` for the client to take a byte. Each
/// socket read from the client waits at most the larger of `link_timeout` and
/// [`ADMITTED_FRAME_TIMEOUT_DURATION`] for a byte until the client's first byte
/// crosses the bridge, and at most `link_timeout` after it. A wait past its
/// limit ends its direction of the bridge. `None` waits with no limit.
/// Example: with a `link_timeout` of 1 second, a client that sends its first
/// request 2 seconds after the session's Hello answer reaches the session, and
/// a client that then sends nothing for 1 second is cut.
///
/// Two threads carry the two directions. Whichever ends first shuts the TCP
/// socket in both directions, ending the thread reading the TLS stream at once,
/// and closes the local connection's read direction. On Unix that ends the
/// thread reading the session at once. A Windows named pipe has no read
/// direction to shut: on Windows that thread ends at the session server's next
/// message or when it hangs up.
///
/// The connection is reported ended once, by whichever direction finishes
/// first.
#[allow(clippy::too_many_arguments)]
fn bridge_remote_connection_to_session(
    reader: TlsReader,
    mut writer: TlsWriter,
    control_stream: TcpStream,
    located_session: &LocatedSession,
    remote_connection_id: u64,
    session_protocol_versions: (u32, u32),
    link_timeout: Option<Duration>,
    dispatcher_events_sender: &Sender<RouterEvent>,
) {
    writer.set_idle_timeout(link_timeout);
    let Some((mut session_reader, mut session_writer, session_read_closer)) =
        open_local_session_bridge_across_restart(located_session, session_protocol_versions)
    else {
        send_refusal_message(
            &mut writer,
            SESSION_NOT_ANSWERING_MESSAGE,
            None,
            Instant::now() + REFUSAL_WINDOW_DURATION,
        );
        report_remote_connection_ended(dispatcher_events_sender, remote_connection_id);
        return;
    };
    let remote_connection_end_report =
        Arc::new(RemoteConnectionEndReport::from_sender_and_connection_id(
            dispatcher_events_sender.clone(),
            remote_connection_id,
        ));

    let Ok(inbound_control_stream) = control_stream.try_clone() else {
        remote_connection_end_report.report_once();
        return;
    };
    let mut remote_reader = reader;
    remote_reader.set_idle_timeout(
        link_timeout.map(|link_timeout| link_timeout.max(ADMITTED_FRAME_TIMEOUT_DURATION)),
    );
    let inbound_end_report = Arc::clone(&remote_connection_end_report);
    let inbound_thread = std::thread::Builder::new()
        .name("koshi-remote-in".to_string())
        .spawn(move || {
            #[cfg(unix)]
            crate::process::block_sigpipe_on_this_thread();
            let mut first_client_byte = [0u8; 1];
            if remote_reader.read_exact(&mut first_client_byte).is_ok()
                && session_writer.write_all(&first_client_byte).is_ok()
            {
                remote_reader.set_idle_timeout(link_timeout);
                let _ = io::copy(&mut remote_reader, &mut session_writer);
            }
            session_read_closer.close();
            let _ = inbound_control_stream.shutdown(Shutdown::Both);
            inbound_end_report.report_once();
        });
    if inbound_thread.is_err() {
        remote_connection_end_report.report_once();
        return;
    }

    let mut remote_writer = writer;
    // This handle stays here; the thread takes its own clone.
    let Ok(outbound_control_stream) = control_stream.try_clone() else {
        let _ = control_stream.shutdown(Shutdown::Both);
        remote_connection_end_report.report_once();
        return;
    };
    let outbound_end_report = Arc::clone(&remote_connection_end_report);
    let outbound_thread = std::thread::Builder::new()
        .name("koshi-remote-out".to_string())
        .spawn(move || {
            #[cfg(unix)]
            crate::process::block_sigpipe_on_this_thread();
            let _ = io::copy(&mut session_reader, &mut remote_writer);
            let _ = outbound_control_stream.shutdown(Shutdown::Both);
            outbound_end_report.report_once();
        });
    if outbound_thread.is_err() {
        // Shutting the socket ends the inbound direction, which is already
        // running.
        let _ = control_stream.shutdown(Shutdown::Both);
        remote_connection_end_report.report_once();
    }
}

/// Report that one admitted connection has ended. It leaves the router's list
/// of live remote connections.
fn report_remote_connection_ended(
    dispatcher_events_sender: &Sender<RouterEvent>,
    remote_connection_id: u64,
) {
    let _ = dispatcher_events_sender.send(RouterEvent::Admission(AdmissionAsk::RemoveConnection {
        remote_connection_id,
    }));
}

/// Reports one bridged connection ended. The first
/// [`RemoteConnectionEndReport::report_once`] sends; every call after it
/// does nothing.
struct RemoteConnectionEndReport {
    /// Where the report goes.
    dispatcher_events_sender: Sender<RouterEvent>,
    /// The number the connection is registered under.
    remote_connection_id: u64,
    /// Set by the first report. Every report after it does nothing.
    has_reported: std::sync::atomic::AtomicBool,
}

impl RemoteConnectionEndReport {
    /// A report for the connection registered under `remote_connection_id`, not yet made.
    fn from_sender_and_connection_id(
        dispatcher_events_sender: Sender<RouterEvent>,
        remote_connection_id: u64,
    ) -> RemoteConnectionEndReport {
        RemoteConnectionEndReport {
            dispatcher_events_sender,
            remote_connection_id,
            has_reported: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Report the connection ended, unless something already has.
    fn report_once(&self) {
        if self.has_reported.swap(true, Ordering::AcqRel) {
            return;
        }
        report_remote_connection_ended(&self.dispatcher_events_sender, self.remote_connection_id);
    }
}

/// Put one question on the dispatcher's queue and wait for its response.
///
/// `None` when the dispatcher is gone or hung up without answering.
fn ask_router_dispatcher<Response>(
    dispatcher_events_sender: &Sender<RouterEvent>,
    build_admission_question: impl FnOnce(Sender<Response>) -> AdmissionAsk,
) -> Option<Response> {
    let (response_sender, response_receiver) = mpsc::channel();
    dispatcher_events_sender
        .send(RouterEvent::Admission(build_admission_question(
            response_sender,
        )))
        .ok()?;
    response_receiver.recv().ok()
}

/// Write one [`REMOTE_REFUSED`] frame, giving the write until `refusal_deadline`.
///
/// A write that fails is dropped, and so is one whose `refusal_deadline` has
/// already passed.
fn send_refusal_with_deadline(writer: &mut (impl Write + Deadlined), refusal_deadline: Instant) {
    send_refusal_message(writer, REMOTE_REFUSED, None, refusal_deadline);
}

/// Write one [`RemoteServerFrame::Refused`] frame carrying `refusal_message`
/// and `settled_remote_protocol_version`, giving the write until
/// `refusal_deadline`.
fn send_refusal_message(
    writer: &mut (impl Write + Deadlined),
    refusal_message: &str,
    settled_remote_protocol_version: Option<u32>,
    refusal_deadline: Instant,
) {
    writer.set_deadline(Some(refusal_deadline));
    let _ = send_remote_frame(
        writer,
        &RemoteServerFrame::Refused {
            message: refusal_message.to_string(),
            remote_protocol_version: settled_remote_protocol_version,
        },
    );
}

/// Write one [`REMOTE_REFUSED`] frame, replacing whatever deadline `writer`
/// holds with [`REFUSAL_WINDOW_DURATION`] counted from now.
///
/// For a caller that is already admitted, whose halves carry no deadline. A
/// caller still inside the admission window is refused with
/// [`send_refusal_with_deadline`], which ends the write at that window's end.
fn send_refusal(writer: &mut (impl Write + Deadlined)) {
    send_refusal_with_deadline(writer, Instant::now() + REFUSAL_WINDOW_DURATION);
}

/// Read one frame: a 4-byte big-endian length, then that many bytes of JSON.
///
/// The length is checked against `maximum_frame_byte_count` before the payload
/// buffer is allocated. Callers pass [`REMOTE_HELLO_MAX_BYTE_COUNT`] before
/// admission and [`MAX_FRAME_BYTE_COUNT`] after it. A length over
/// `maximum_frame_byte_count` is [`RemoteClientFrameRead::Closed`] and reads no
/// payload. A payload that is not a [`RemoteClientFrame`] and reads as a
/// [`PreviousReleaseRemoteClientFrame`] is [`RemoteClientFrameRead::PreviousReleaseHello`].
fn read_client_frame<Reader: Read>(
    reader: &mut Reader,
    maximum_frame_byte_count: u32,
) -> RemoteClientFrameRead {
    let mut length_bytes = [0u8; 4];
    if reader.read_exact(&mut length_bytes).is_err() {
        return RemoteClientFrameRead::Closed;
    }
    let payload_byte_count = u32::from_be_bytes(length_bytes);
    if payload_byte_count > maximum_frame_byte_count {
        return RemoteClientFrameRead::Closed;
    }
    let mut payload_bytes = vec![0u8; payload_byte_count as usize];
    if reader.read_exact(&mut payload_bytes).is_err() {
        return RemoteClientFrameRead::Closed;
    }
    if let Ok(remote_client_frame) = serde_json::from_slice(&payload_bytes) {
        return RemoteClientFrameRead::Frame(remote_client_frame);
    }
    match serde_json::from_slice(&payload_bytes) {
        Ok(PreviousReleaseRemoteClientFrame::Hello {
            minimum_remote_version,
            maximum_remote_version,
        }) => RemoteClientFrameRead::PreviousReleaseHello {
            minimum_remote_version,
            maximum_remote_version,
        },
        Err(_) => RemoteClientFrameRead::Unreadable,
    }
}

/// Serialize `remote_server_frame` as one frame and write all its bytes.
///
/// The frame has a 4-byte big-endian length followed by the JSON payload.
///
/// # Errors
/// The JSON encoder's own failure, `the answer is larger than a frame can
/// carry` for a payload past `u32::MAX` bytes, and whatever the writer reports.
fn send_remote_frame<Writer: Write>(
    writer: &mut Writer,
    remote_server_frame: &RemoteServerFrame,
) -> io::Result<()> {
    let payload_bytes = serde_json::to_vec(remote_server_frame)?;
    let frame_byte_count = u32::try_from(payload_bytes.len())
        .map_err(|_| io::Error::other("the answer is larger than a frame can carry"))?;
    let mut frame_bytes = Vec::with_capacity(payload_bytes.len() + 4);
    frame_bytes.extend_from_slice(&frame_byte_count.to_be_bytes());
    frame_bytes.extend_from_slice(&payload_bytes);
    writer.write_all(&frame_bytes)
}

#[cfg(test)]
mod tests;
