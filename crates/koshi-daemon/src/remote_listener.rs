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
//! registered until this listener reports it ended. A revoke shuts a registered
//! connection's socket, attached or not. The router holds at most
//! [`MAX_LIVE_REMOTE_CONNECTION_COUNT`](crate::router::MAX_LIVE_REMOTE_CONNECTION_COUNT) registrations and
//! refuses the connections that arrive over that count.
//!
//! The TLS handshake, the frame the caller opens with, and the refusal naming
//! both version ranges finish inside `ADMISSION_WINDOW_DURATION`, counted from the
//! moment the connection's thread starts. Each single read and write inside
//! them is given the time left on that deadline when it starts. Every other
//! refusal replaces that deadline with `REFUSAL_WINDOW_DURATION`. After the Welcome both
//! halves lose their deadline.
//!
//! Every refusal is
//! [`REMOTE_REFUSED`](koshi_ipc::remote_wire::REMOTE_REFUSED) and closes the
//! connection. A wrong secret, a revoked secret, a session that does not exist,
//! a session the secret holds no grant for, and a session another local user
//! started produce the same bytes and the same work: no caller-supplied name
//! reaches a socket connect, a wait, or a file until the admitted scope has
//! been proven to cover it. Order is `admit` → `resolve` → `covers` →
//! `started_by_this_router` → open.
//!
//! This listener carries the three remote frames and then one session server's
//! own bytes. No path from it reaches the router's control plane, so
//! `koshi share` is unreachable over a remote connection. A client counts as
//! remote when this listener accepted it: the router marks the Hello it sends
//! the session server on that client's behalf. A caller can add that mark to
//! itself and cannot take it off.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection};

use koshi_ipc::endpoint::EndpointFile;
use koshi_ipc::protocol::{
    compute_agreed_protocol_version, ConnectionToken, IpcRequest, IpcRequestKind,
};
use koshi_ipc::remote_state::CertFile;
use koshi_ipc::remote_tokens::TokenScope;
use koshi_ipc::remote_wire::{
    format_version_refusal, RemoteClientFrame, RemoteServerFrame, RemoteSessionRow,
    MIN_REMOTE_PROTOCOL_VERSION, REMOTE_HELLO_MAX_BYTE_COUNT, REMOTE_PROTOCOL_VERSION,
    REMOTE_REFUSED,
};
use koshi_ipc::router::SessionSelector;
use koshi_ipc::tls::{self, TlsReader, TlsWriter};
use koshi_ipc::transport::{
    Connection, Deadlined, RawReader, RawWriter, ReadCloser, MAX_FRAME_BYTE_COUNT,
};

use crate::router::RouterEvent;

/// How long the connection's thread spends on the TLS handshake, on reading
/// the frame the caller opens with, and on writing every refusal it answers
/// before admission, counted from the moment that thread starts. A caller that
/// is not admitted holds its thread and its admission place for no longer than
/// this.
const ADMISSION_WINDOW_DURATION: Duration = Duration::from_secs(10);

/// How long one address's connection attempts are counted over.
const RATE_WINDOW_DURATION: Duration = Duration::from_secs(60);

/// How many connections one address may open inside [`RATE_WINDOW_DURATION`] before
/// the rest are dropped.
const MAX_ATTEMPT_COUNT: u32 = 10;

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

/// How often one repeated warning about this port is written at most.
pub(crate) const LOG_WINDOW_DURATION: Duration = Duration::from_secs(60);

/// How long a refusal has to reach the caller it answers, counted from the
/// write. A refusal written before admission is cut short at the admission
/// deadline instead.
const REFUSAL_WINDOW_DURATION: Duration = Duration::from_secs(10);

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
        response_sender: Sender<Option<Admitted>>,
    },
    /// The sessions an admitted scope reaches.
    Rows {
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
        /// Where the answer goes. `None` refuses the attach.
        response_sender: Sender<Option<PathBuf>>,
    },
    /// One admitted connection has ended. It leaves the router's list.
    Ended {
        /// The number that connection was registered under.
        remote_connection_id: u64,
    },
}

/// What a presented secret reached.
pub(crate) struct Admitted {
    /// How far the grant behind that secret reaches.
    pub scope: TokenScope,
    /// The number this connection is registered under, named again when it
    /// attaches and when it ends.
    pub remote_connection_id: u64,
}

/// A TLS port this machine holds and is not yet serving on.
///
/// Dropping this without calling [`Bound::start_serving`] gives the port back.
pub(crate) struct Bound {
    /// Sends the accept loop what it needs to start. Dropping this without
    /// sending ends the waiting thread, which gives the port back.
    dispatcher_sender: Sender<Sender<RouterEvent>>,
}

/// Take the TLS port at `remote_listen_address`, presenting `certificate_file`, without serving on it
/// yet.
///
/// Builds the TLS configuration, binds `remote_listen_address`, and starts the accept thread.
/// That thread holds the port and accepts nobody until [`Bound::start_serving`] sends it
/// somewhere to put its questions, or until the sender is dropped, which ends it
/// and releases the port.
///
/// # Errors
/// The certificate that could not be turned into a TLS configuration, or the
/// address that could not be bound.
pub(crate) fn bind_remote_listener(
    remote_listen_address: String,
    certificate_file: &CertFile,
) -> io::Result<Bound> {
    let tls_config = Arc::new(build_server_config(certificate_file)?);
    let listener = TcpListener::bind(&remote_listen_address)?;
    let (dispatcher_sender, dispatcher_receiver) = mpsc::channel::<Sender<RouterEvent>>();
    std::thread::Builder::new()
        .name("koshi-remote-accept".to_string())
        .spawn(move || {
            let Ok(dispatcher_events_sender) = dispatcher_receiver.recv() else {
                return;
            };
            run_remote_accept_loop(&listener, &tls_config, &dispatcher_events_sender);
        })?;
    Ok(Bound { dispatcher_sender })
}

impl Bound {
    /// Start serving on this port. The thread [`bind_remote_listener`] started begins accepting
    /// connections and gives each its own thread; `dispatcher_events_sender` carries those
    /// threads' questions to the router's dispatcher.
    ///
    /// Cannot fail.
    pub(crate) fn start_serving(self, dispatcher_events_sender: Sender<RouterEvent>) {
        let _ = self.dispatcher_sender.send(dispatcher_events_sender);
    }
}

/// The TLS configuration this machine serves with: `cert`'s certificate and
/// private key, and no client certificate asked for.
fn build_server_config(certificate_file: &CertFile) -> io::Result<ServerConfig> {
    let certificate_chain = vec![CertificateDer::from(certificate_file.cert_der.clone())];
    let private_key =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certificate_file.key_der.clone()));
    ServerConfig::builder_with_provider(koshi_ipc::tls::build_crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports every default protocol version")
        .with_no_client_auth()
        .with_single_cert(certificate_chain, private_key)
        .map_err(|certificate_error| io::Error::other(certificate_error.to_string()))
}

/// One repeated warning, written at most once inside [`LOG_WINDOW_DURATION`].
pub(crate) struct WarningRateLimiter {
    /// When the last line was written, or `None` when none has been.
    last_warning_at: Option<Instant>,
}

impl WarningRateLimiter {
    /// A warning that has not been written yet.
    pub(crate) fn new() -> WarningRateLimiter {
        Self {
            last_warning_at: None,
        }
    }

    /// Whether to write the line at `current_time`. True when no line has been written,
    /// and when the last one was written [`LOG_WINDOW_DURATION`] or longer ago. Writing
    /// is the caller's; this only answers.
    ///
    /// Example — with [`LOG_WINDOW_DURATION`] at 60 seconds, ten thousand calls spread
    /// over five minutes answer true five times.
    pub(crate) fn is_due(&mut self, current_time: Instant) -> bool {
        if self.last_warning_at.is_some_and(|warning_written_at| {
            current_time.duration_since(warning_written_at) < LOG_WINDOW_DURATION
        }) {
            return false;
        }
        self.last_warning_at = Some(current_time);
        true
    }
}

/// Accept connections and give each its own thread, dropping the ones from an
/// address that has opened more than [`MAX_ATTEMPT_COUNT`] inside [`RATE_WINDOW_DURATION`]
/// and the ones that arrive while [`MAX_ADMISSION_COUNT`] connections are already
/// waiting to present a secret. A failed accept is reported at most once inside
/// [`LOG_WINDOW_DURATION`], waits [`ACCEPT_RETRY_DELAY_DURATION`], and retries.
fn run_remote_accept_loop(
    listener: &TcpListener,
    tls_config: &Arc<ServerConfig>,
    dispatcher_events_sender: &Sender<RouterEvent>,
) {
    let mut rate_table = RateTable::new();
    let admission_count = Arc::new(AtomicUsize::new(0));
    let mut full_admission_warning = WarningRateLimiter::new();
    let mut accept_failure_warning = WarningRateLimiter::new();
    loop {
        let (tcp_stream, peer_address) = match listener.accept() {
            Ok(accepted_connection) => accepted_connection,
            Err(accept_error) => {
                if accept_failure_warning.is_due(Instant::now()) {
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
        let peer_ip_address = peer_address.ip();
        match rate_table.decide_attempt(peer_ip_address, current_time) {
            Attempt::Serve => {}
            Attempt::DropAndSay => {
                tracing::warn!(
                    %peer_ip_address,
                    "remote connection attempts from {peer_ip_address} exceeded {MAX_ATTEMPT_COUNT} in \
                     {RATE_WINDOW_DURATION:?}; dropping the rest until the window passes"
                );
                drop(tcp_stream);
                continue;
            }
            Attempt::DropInSilence => {
                drop(tcp_stream);
                continue;
            }
        }
        let Some(admission_slot) = AdmissionSlot::enter_admission(&admission_count) else {
            if full_admission_warning.is_due(current_time) {
                tracing::warn!(
                    "{MAX_ADMISSION_COUNT} remote connections are waiting to present a secret; \
                     closing the ones that arrive until some of them finish"
                );
            }
            drop(tcp_stream);
            continue;
        };
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
                )
            });
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
enum Attempt {
    /// Serve it: this address is inside its limit.
    Serve,
    /// Drop it, and log that the address crossed its limit. This is the first
    /// attempt over the limit in this window.
    DropAndSay,
    /// Drop it without logging. This address crossed its limit earlier in the
    /// same window.
    DropInSilence,
}

/// What one address has done inside the window it opened.
struct RateWindow {
    /// How many connections that address has opened since `opened`.
    attempt_count: u32,
    /// When the first of them arrived.
    window_started_at: Instant,
}

/// How many connections each address has opened lately.
///
/// Bounded at [`MAX_RATE_TABLE_ENTRY_COUNT`]. Every check first drops the addresses whose
/// window has passed; a check that still finds the table full drops the address
/// whose window opened first.
struct RateTable {
    /// One window per address.
    window_by_peer_ip_address: HashMap<IpAddr, RateWindow>,
}

impl RateTable {
    /// An empty table.
    fn new() -> RateTable {
        RateTable {
            window_by_peer_ip_address: HashMap::new(),
        }
    }

    /// Count one connection from `peer_ip_address` at `current_time` and say what to do with it.
    ///
    /// An address is logged once per window, on the attempt that crosses
    /// [`MAX_ATTEMPT_COUNT`]. Every subsequent attempt in that window is dropped in
    /// silence.
    ///
    /// Example — with [`MAX_ATTEMPT_COUNT`] at 10, attempts 1 to 10 from one
    /// address are [`Attempt::Serve`], attempt 11 is [`Attempt::DropAndSay`],
    /// and attempts 12 onward are [`Attempt::DropInSilence`] until the window
    /// passes.
    fn decide_attempt(&mut self, peer_ip_address: IpAddr, current_time: Instant) -> Attempt {
        self.window_by_peer_ip_address.retain(|_, window| {
            current_time.duration_since(window.window_started_at) < RATE_WINDOW_DURATION
        });
        if self.window_by_peer_ip_address.len() >= MAX_RATE_TABLE_ENTRY_COUNT
            && !self
                .window_by_peer_ip_address
                .contains_key(&peer_ip_address)
        {
            let oldest_peer_ip_address = self
                .window_by_peer_ip_address
                .iter()
                .min_by_key(|(_, window)| window.window_started_at)
                .map(|(peer_ip_address, _)| *peer_ip_address);
            if let Some(oldest_peer_ip_address) = oldest_peer_ip_address {
                self.window_by_peer_ip_address
                    .remove(&oldest_peer_ip_address);
            }
        }
        let peer_window = self
            .window_by_peer_ip_address
            .entry(peer_ip_address)
            .or_insert(RateWindow {
                attempt_count: 0,
                window_started_at: current_time,
            });
        peer_window.attempt_count += 1;
        match peer_window.attempt_count {
            attempt_count if attempt_count <= MAX_ATTEMPT_COUNT => Attempt::Serve,
            attempt_count if attempt_count == MAX_ATTEMPT_COUNT + 1 => Attempt::DropAndSay,
            _ => Attempt::DropInSilence,
        }
    }
}

/// What the frame a caller sends turned out to be.
enum Opening {
    /// A readable frame.
    Frame(RemoteClientFrame),
    /// Bytes that are not a readable frame. The caller is refused.
    Unreadable,
    /// A length prefix past the cap, or a stream that ended or timed out.
    /// Nothing is written back.
    Closed,
}

/// Serve one remote connection: the TLS handshake, the secret, and then either
/// the sessions that secret reaches or a bridge to one of them.
///
/// The TLS handshake and the frame the caller opens with finish inside
/// [`ADMISSION_WINDOW_DURATION`], counted from the moment this thread starts. A refusal
/// written by [`send_refusal`] gets [`REFUSAL_WINDOW_DURATION`] instead. Once the caller is
/// admitted both halves and the socket lose their deadlines and block for as
/// long as it takes.
///
/// Admission registers the connection with the router, attached or not. The
/// registration is dropped when the connection finishes, whichever step it
/// finished at.
///
/// `admission_slot` holds this connection's place in the admission window and is
/// dropped the moment the secret is admitted.
///
/// On Unix the thread blocks SIGPIPE on its own signal mask; a write to a peer
/// that hung up returns an error whatever the process-wide disposition is.
fn serve_remote_connection(
    mut tcp_stream: TcpStream,
    tls_config: &Arc<ServerConfig>,
    dispatcher_events_sender: &Sender<RouterEvent>,
    admission_slot: AdmissionSlot,
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
        Opening::Frame(RemoteClientFrame::Hello {
            min_remote_version,
            max_remote_version,
            min_protocol_version,
            max_protocol_version,
            connection_token,
        }) => (
            (min_remote_version, max_remote_version),
            (min_protocol_version, max_protocol_version),
            connection_token,
        ),
        Opening::Frame(_) | Opening::Unreadable => {
            send_refusal_with_deadline(&mut writer, compute_refusal_deadline(admission_deadline));
            return;
        }
        Opening::Closed => return,
    };

    // The version is settled before the secret is looked at. This refusal names
    // both ranges instead of carrying REMOTE_REFUSED.
    let Some(remote_version) = compute_agreed_protocol_version(
        minimum_remote_version,
        maximum_remote_version,
        MIN_REMOTE_PROTOCOL_VERSION,
        REMOTE_PROTOCOL_VERSION,
    ) else {
        let _ = send_remote_frame(
            &mut writer,
            &RemoteServerFrame::Refused {
                message: format_version_refusal(minimum_remote_version, maximum_remote_version),
            },
        );
        return;
    };

    let Ok(registered_stream) = control_stream.try_clone() else {
        return;
    };
    let admission_result = ask_router_dispatcher(dispatcher_events_sender, |response_sender| {
        AdmissionAsk::Admit {
            connection_token,
            remote_connection_stream: registered_stream,
            response_sender,
        }
    });
    let Some(Some(admitted_connection)) = admission_result else {
        send_refusal_with_deadline(&mut writer, compute_refusal_deadline(admission_deadline));
        return;
    };

    // The caller leaves the admission window.
    drop(admission_slot);

    // Both halves and the socket lose their deadlines.
    reader.set_deadline(None);
    writer.set_deadline(None);
    let _ = control_stream.set_read_timeout(None);
    let _ = control_stream.set_write_timeout(None);
    if send_remote_frame(
        &mut writer,
        &RemoteServerFrame::Welcome {
            remote_protocol_version: remote_version,
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
/// `session_protocol_versions` is the session protocol range the client named in its opening
/// frame. Nothing here reads it: it is carried to
/// [`bridge_remote_connection_to_session`], which puts it in the session-plane Hello it sends
/// for this client, so the client and the session server settle a version
/// between themselves.
fn serve_admitted_remote_connection(
    mut reader: TlsReader,
    mut writer: TlsWriter,
    control_stream: TcpStream,
    admitted_connection: Admitted,
    session_protocol_versions: (u32, u32),
    dispatcher_events_sender: &Sender<RouterEvent>,
) {
    let attached_session_endpoint_path = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        dispatcher_events_sender,
    );
    match attached_session_endpoint_path {
        Some(session_endpoint_path) => bridge_remote_connection_to_session(
            reader,
            writer,
            control_stream,
            session_endpoint_path,
            admitted_connection.remote_connection_id,
            session_protocol_versions,
            dispatcher_events_sender,
        ),
        None => report_remote_connection_ended(
            dispatcher_events_sender,
            admitted_connection.remote_connection_id,
        ),
    }
}

/// Read the frames an admitted connection sends until it attaches or ends.
///
/// A list is answered and the next frame is read, so one connection may list
/// and then attach. `Some` is the endpoint file of the session an admitted
/// attach reached; the bytes after that attach belong to that session's server.
/// `None` means the connection is finished: it hung up, it sent something this
/// loop does not serve, its attach was refused, or the dispatcher is gone.
fn process_admitted_remote_frames(
    reader: &mut impl Read,
    writer: &mut (impl Write + Deadlined),
    admitted_connection: &Admitted,
    dispatcher_events_sender: &Sender<RouterEvent>,
) -> Option<PathBuf> {
    loop {
        let remote_client_frame = match read_client_frame(reader, MAX_FRAME_BYTE_COUNT) {
            Opening::Frame(remote_client_frame) => remote_client_frame,
            Opening::Unreadable => {
                send_refusal(writer);
                return None;
            }
            Opening::Closed => return None,
        };
        match remote_client_frame {
            RemoteClientFrame::List => {
                let admitted_token_scope = admitted_connection.scope.clone();
                let remote_session_rows =
                    ask_router_dispatcher(dispatcher_events_sender, |response_sender| {
                        AdmissionAsk::Rows {
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
            RemoteClientFrame::Attach { session_selector } => {
                let admitted_token_scope = admitted_connection.scope.clone();
                let remote_connection_id = admitted_connection.remote_connection_id;
                let located_session_endpoint_path =
                    ask_router_dispatcher(dispatcher_events_sender, |response_sender| {
                        AdmissionAsk::Locate {
                            scope: admitted_token_scope,
                            remote_connection_id,
                            session_selector,
                            response_sender,
                        }
                    })?;
                let Some(session_endpoint_path) = located_session_endpoint_path else {
                    send_refusal(writer);
                    return None;
                };
                return Some(session_endpoint_path);
            }
            RemoteClientFrame::Hello { .. } => {
                send_refusal(writer);
                return None;
            }
        }
    }
}

/// The Hello the router sends a session server for a caller this listener
/// accepted: `connection_token` from that session's endpoint file, the caller's own
/// version range in `session_protocol_versions` as `(minimum, maximum)`, and `remote` set.
///
/// This is the only place `remote` is set.
fn build_bridged_hello(
    connection_token: ConnectionToken,
    session_protocol_versions: (u32, u32),
) -> IpcRequest {
    let (minimum_protocol_version, maximum_protocol_version) = session_protocol_versions;
    IpcRequest {
        request_id: 1,
        request_kind: IpcRequestKind::Hello {
            min_protocol_version: minimum_protocol_version,
            max_protocol_version: maximum_protocol_version,
            connection_token,
            is_remote: true,
        },
    }
}

/// Open the local connection to the session advertised at `endpoint_path` and
/// send it the Hello carrying that session's endpoint token and `session_protocol_versions`.
///
/// Hands back the connection's two raw halves and the handle that closes its
/// read direction.
///
/// `None` when the endpoint file cannot be read, its socket cannot be reached,
/// the read direction cannot be made closable, or the Hello cannot be sent.
fn open_local_session_bridge(
    session_endpoint_path: &Path,
    session_protocol_versions: (u32, u32),
) -> Option<(RawReader, RawWriter, ReadCloser)> {
    let session_endpoint = EndpointFile::load_from_path(session_endpoint_path).ok()?;
    let mut session_connection = Connection::connect(&session_endpoint.socket_address).ok()?;
    let session_read_closer = session_connection.read_closer().ok()?;
    session_connection
        .send(&build_bridged_hello(
            session_endpoint.connection_token,
            session_protocol_versions,
        ))
        .ok()?;
    let (session_reader, session_writer) = session_connection.split_raw();
    Some((session_reader, session_writer, session_read_closer))
}

/// Open the local connection to an admitted client's session and carry the
/// bytes both ways.
///
/// The router sends the session-plane Hello carrying the session's endpoint
/// token and the client's version range. The session server's answer to that
/// Hello, and everything after it, travels back through the bridge unread. A
/// session that cannot be opened is refused, and the connection is reported
/// ended with nothing bridged.
///
/// Two threads carry the two directions. Whichever ends first shuts the TCP
/// socket in both directions, ending the thread reading the TLS stream at once,
/// and closes the local connection's read direction. On Unix that ends the
/// thread reading the session at once. A Windows named pipe carries no read
/// direction to shut, so there that thread ends at the session server's next
/// message or when it hangs up.
///
/// The connection is reported ended once, by whichever direction finishes
/// first.
fn bridge_remote_connection_to_session(
    reader: TlsReader,
    mut writer: TlsWriter,
    control_stream: TcpStream,
    session_endpoint_path: PathBuf,
    remote_connection_id: u64,
    session_protocol_versions: (u32, u32),
    dispatcher_events_sender: &Sender<RouterEvent>,
) {
    let Some((mut session_reader, mut session_writer, session_read_closer)) =
        open_local_session_bridge(&session_endpoint_path, session_protocol_versions)
    else {
        send_refusal(&mut writer);
        report_remote_connection_ended(dispatcher_events_sender, remote_connection_id);
        return;
    };
    let end_report = Arc::new(EndReport::from_sender_and_connection_id(
        dispatcher_events_sender.clone(),
        remote_connection_id,
    ));

    let Ok(inbound_control_stream) = control_stream.try_clone() else {
        end_report.report_once();
        return;
    };
    let mut remote_reader = reader;
    let inbound_end_report = Arc::clone(&end_report);
    let inbound_thread = std::thread::Builder::new()
        .name("koshi-remote-in".to_string())
        .spawn(move || {
            #[cfg(unix)]
            crate::process::block_sigpipe_on_this_thread();
            let _ = io::copy(&mut remote_reader, &mut session_writer);
            session_read_closer.close();
            let _ = inbound_control_stream.shutdown(Shutdown::Both);
            inbound_end_report.report_once();
        });
    if inbound_thread.is_err() {
        end_report.report_once();
        return;
    }

    let mut remote_writer = writer;
    // This handle stays here; the thread takes its own clone.
    let Ok(outbound_control_stream) = control_stream.try_clone() else {
        let _ = control_stream.shutdown(Shutdown::Both);
        end_report.report_once();
        return;
    };
    let outbound_end_report = Arc::clone(&end_report);
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
        end_report.report_once();
    }
}

/// Report that one admitted connection has ended. It leaves the router's list
/// of live remote connections.
fn report_remote_connection_ended(
    dispatcher_events_sender: &Sender<RouterEvent>,
    remote_connection_id: u64,
) {
    let _ = dispatcher_events_sender.send(RouterEvent::Admission(AdmissionAsk::Ended {
        remote_connection_id,
    }));
}

/// Reports one bridged connection ended. The first [`EndReport::report_once`] sends;
/// every subsequent one does nothing.
struct EndReport {
    /// Where the report goes.
    dispatcher_events_sender: Sender<RouterEvent>,
    /// The number the connection is registered under.
    remote_connection_id: u64,
    /// Set by the first report. Every subsequent one does nothing.
    has_reported: std::sync::atomic::AtomicBool,
}

impl EndReport {
    /// A report for the connection registered under `remote_connection_id`, not yet made.
    fn from_sender_and_connection_id(
        dispatcher_events_sender: Sender<RouterEvent>,
        remote_connection_id: u64,
    ) -> EndReport {
        EndReport {
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
/// A write that fails is dropped, and so is one whose `refusal_deadline` has already
/// passed.
fn send_refusal_with_deadline(writer: &mut (impl Write + Deadlined), refusal_deadline: Instant) {
    writer.set_deadline(Some(refusal_deadline));
    let _ = send_remote_frame(
        writer,
        &RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        },
    );
}

/// Write one [`REMOTE_REFUSED`] frame, replacing whatever deadline `writer`
/// holds with [`REFUSAL_WINDOW_DURATION`] counted from now.
///
/// For a caller that is already admitted, whose halves carry no deadline. A
/// caller still inside the admission window is refused with
/// [`send_refusal_with_deadline`],
/// which cannot hold a thread past that window.
fn send_refusal(writer: &mut (impl Write + Deadlined)) {
    send_refusal_with_deadline(writer, Instant::now() + REFUSAL_WINDOW_DURATION);
}

/// Read one frame: a 4-byte big-endian length, then that many bytes of JSON.
///
/// The length is checked against `maximum_frame_byte_count` before the payload buffer is
/// allocated. Callers pass [`REMOTE_HELLO_MAX_BYTE_COUNT`] before admission and
/// [`MAX_FRAME_BYTE_COUNT`] after it. A length over `maximum_frame_byte_count` is [`Opening::Closed`]
/// and reads no payload.
fn read_client_frame<R: Read>(reader: &mut R, maximum_frame_byte_count: u32) -> Opening {
    let mut length_bytes = [0u8; 4];
    if reader.read_exact(&mut length_bytes).is_err() {
        return Opening::Closed;
    }
    let payload_byte_count = u32::from_be_bytes(length_bytes);
    if payload_byte_count > maximum_frame_byte_count {
        return Opening::Closed;
    }
    let mut payload_bytes = vec![0u8; payload_byte_count as usize];
    if reader.read_exact(&mut payload_bytes).is_err() {
        return Opening::Closed;
    }
    match serde_json::from_slice(&payload_bytes) {
        Ok(remote_client_frame) => Opening::Frame(remote_client_frame),
        Err(_) => Opening::Unreadable,
    }
}

/// Serialize `remote_server_frame` as one frame and write all its bytes.
///
/// The frame has a 4-byte big-endian length followed by the JSON payload.
///
/// # Errors
/// The JSON encoder's own failure, `the answer is larger than a frame can
/// carry` for a payload past `u32::MAX` bytes, and whatever the writer reports.
fn send_remote_frame<W: Write>(
    writer: &mut W,
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
