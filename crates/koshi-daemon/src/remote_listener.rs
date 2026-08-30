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
//! [`MAX_LIVE_REMOTE`](crate::router::MAX_LIVE_REMOTE) registrations and
//! refuses the connections that arrive over that count.
//!
//! The TLS handshake, the frame the caller opens with, and the refusal naming
//! both version ranges finish inside `ADMISSION_WINDOW`, counted from the
//! moment the connection's thread starts. Each single read and write inside
//! them is given the time left on that deadline when it starts. Every other
//! refusal replaces that deadline with `REFUSAL_WINDOW`. After the Welcome both
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
use koshi_ipc::protocol::{agreed_version, ConnectionToken, IpcRequest, IpcRequestKind};
use koshi_ipc::remote_state::CertFile;
use koshi_ipc::remote_tokens::TokenScope;
use koshi_ipc::remote_wire::{
    version_refusal, RemoteClientFrame, RemoteServerFrame, RemoteSessionRow,
    MIN_REMOTE_PROTOCOL_VERSION, REMOTE_HELLO_MAX_LEN, REMOTE_PROTOCOL_VERSION, REMOTE_REFUSED,
};
use koshi_ipc::router::SessionSelector;
use koshi_ipc::tls::{self, TlsReader, TlsWriter};
use koshi_ipc::transport::{
    Connection, Deadlined, RawReader, RawWriter, ReadCloser, MAX_FRAME_LEN,
};

use crate::router::RouterEvent;

/// How long the connection's thread spends on the TLS handshake, on reading
/// the frame the caller opens with, and on writing every refusal it answers
/// before admission, counted from the moment that thread starts. A caller that
/// is not admitted holds its thread and its admission place for no longer than
/// this.
const ADMISSION_WINDOW: Duration = Duration::from_secs(10);

/// How long one address's connection attempts are counted over.
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// How many connections one address may open inside [`RATE_WINDOW`] before
/// the rest are dropped.
const MAX_ATTEMPTS: u32 = 10;

/// How many addresses the attempt table counts at once.
const MAX_ENTRIES: usize = 1024;

/// How many connections may be inside the admission window at once, across
/// every address.
///
/// A connection is counted from the moment it is accepted until its secret is
/// admitted or it goes away. One arriving over this count is closed without a
/// handshake. An admitted connection is not counted here; it counts against
/// [`MAX_LIVE_REMOTE`](crate::router::MAX_LIVE_REMOTE) instead.
const MAX_IN_ADMISSION: usize = 64;

/// How long the accept loop pauses after a failed accept before trying again.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// How often one repeated warning about this port is written at most.
pub(crate) const LOG_WINDOW: Duration = Duration::from_secs(60);

/// How long a refusal has to reach the caller it answers, counted from the
/// write. A refusal written before admission is cut short at the admission
/// deadline instead.
const REFUSAL_WINDOW: Duration = Duration::from_secs(10);

/// When a refusal written inside the admission window must be on the socket:
/// [`REFUSAL_WINDOW`] from now, or `deadline`, whichever comes first.
fn refusal_deadline(deadline: Instant) -> Instant {
    deadline.min(Instant::now() + REFUSAL_WINDOW)
}

/// One question a connection thread puts to the router's dispatcher.
pub(crate) enum AdmissionAsk {
    /// What a presented secret reaches. A secret that reaches something
    /// registers this connection with the router.
    Admit {
        /// The secret the caller presented.
        token: ConnectionToken,
        /// The connection's socket. A revoke shuts it.
        stream: TcpStream,
        /// Where the answer goes. `None` refuses the connection.
        reply: Sender<Option<Admitted>>,
    },
    /// The sessions an admitted scope reaches.
    Rows {
        /// How far the admitting grant reaches.
        scope: TokenScope,
        /// Where the answer goes.
        reply: Sender<Vec<RemoteSessionRow>>,
    },
    /// Which session a selector names, when this connection still stands and
    /// the admitted scope covers it.
    Locate {
        /// How far the admitting grant reaches.
        scope: TokenScope,
        /// The number this connection is registered under.
        id: u64,
        /// The session the caller named.
        selector: SessionSelector,
        /// Where the answer goes. `None` refuses the attach.
        reply: Sender<Option<PathBuf>>,
    },
    /// One admitted connection has ended. It leaves the router's list.
    Ended {
        /// The number that connection was registered under.
        id: u64,
    },
}

/// What a presented secret reached.
pub(crate) struct Admitted {
    /// How far the grant behind that secret reaches.
    pub scope: TokenScope,
    /// The number this connection is registered under, named again when it
    /// attaches and when it ends.
    pub id: u64,
}

/// A TLS port this machine holds and is not yet serving on.
///
/// Dropping this without calling [`Bound::serve`] gives the port back.
pub(crate) struct Bound {
    /// Sends the accept loop what it needs to start. Dropping this without
    /// sending ends the waiting thread, which gives the port back.
    go: Sender<Sender<RouterEvent>>,
}

/// Take the TLS port at `address`, presenting `cert`, without serving on it
/// yet.
///
/// Builds the TLS configuration, binds `address`, and starts the accept thread.
/// That thread holds the port and accepts nobody until [`Bound::serve`] sends it
/// somewhere to put its questions, or until the sender is dropped, which ends it
/// and releases the port.
///
/// # Errors
/// The certificate that could not be turned into a TLS configuration, or the
/// address that could not be bound.
pub(crate) fn bind(address: String, cert: &CertFile) -> io::Result<Bound> {
    let tls = Arc::new(server_config(cert)?);
    let listener = TcpListener::bind(&address)?;
    let (go, wait) = mpsc::channel::<Sender<RouterEvent>>();
    std::thread::Builder::new()
        .name("koshi-remote-accept".to_string())
        .spawn(move || {
            let Ok(admissions) = wait.recv() else {
                return;
            };
            accept_loop(&listener, &tls, &admissions);
        })?;
    Ok(Bound { go })
}

impl Bound {
    /// Start serving on this port. The thread [`bind`] started begins accepting
    /// connections and gives each its own thread; `admissions` carries those
    /// threads' questions to the router's dispatcher.
    ///
    /// Cannot fail.
    pub(crate) fn serve(self, admissions: Sender<RouterEvent>) {
        let _ = self.go.send(admissions);
    }
}

/// The TLS configuration this machine serves with: `cert`'s certificate and
/// private key, and no client certificate asked for.
fn server_config(cert: &CertFile) -> io::Result<ServerConfig> {
    let chain = vec![CertificateDer::from(cert.cert_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_der.clone()));
    ServerConfig::builder_with_provider(koshi_ipc::tls::crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports every default protocol version")
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|error| io::Error::other(error.to_string()))
}

/// One repeated warning, written at most once inside [`LOG_WINDOW`].
pub(crate) struct Occasional {
    /// When the last line was written, or `None` when none has been.
    said_at: Option<Instant>,
}

impl Occasional {
    /// A warning that has not been written yet.
    pub(crate) fn new() -> Occasional {
        Occasional { said_at: None }
    }

    /// Whether to write the line at `now`. True when no line has been written,
    /// and when the last one was written [`LOG_WINDOW`] or longer ago. Writing
    /// is the caller's; this only answers.
    ///
    /// Example — with [`LOG_WINDOW`] at 60 seconds, ten thousand calls spread
    /// over five minutes answer true five times.
    pub(crate) fn due(&mut self, now: Instant) -> bool {
        if self
            .said_at
            .is_some_and(|said| now.duration_since(said) < LOG_WINDOW)
        {
            return false;
        }
        self.said_at = Some(now);
        true
    }
}

/// Accept connections and give each its own thread, dropping the ones from an
/// address that has opened more than [`MAX_ATTEMPTS`] inside [`RATE_WINDOW`]
/// and the ones that arrive while [`MAX_IN_ADMISSION`] connections are already
/// waiting to present a secret. A failed accept is reported at most once inside
/// [`LOG_WINDOW`], waits [`ACCEPT_RETRY_DELAY`], and retries.
fn accept_loop(listener: &TcpListener, tls: &Arc<ServerConfig>, admissions: &Sender<RouterEvent>) {
    let mut attempts = RateTable::new();
    let in_admission = Arc::new(AtomicUsize::new(0));
    let mut refused_full = Occasional::new();
    let mut failed_accept = Occasional::new();
    loop {
        let (sock, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) => {
                if failed_accept.due(Instant::now()) {
                    tracing::warn!(
                        %error,
                        "the remote port could not accept a connection; \
                         retrying every {ACCEPT_RETRY_DELAY:?}"
                    );
                }
                std::thread::sleep(ACCEPT_RETRY_DELAY);
                continue;
            }
        };
        let now = Instant::now();
        let ip = peer.ip();
        match attempts.allow(ip, now) {
            Attempt::Serve => {}
            Attempt::DropAndSay => {
                tracing::warn!(
                    %ip,
                    "remote connection attempts from {ip} exceeded {MAX_ATTEMPTS} in \
                     {RATE_WINDOW:?}; dropping the rest until the window passes"
                );
                drop(sock);
                continue;
            }
            Attempt::DropInSilence => {
                drop(sock);
                continue;
            }
        }
        let Some(counted) = InAdmission::enter(&in_admission) else {
            if refused_full.due(now) {
                tracing::warn!(
                    "{MAX_IN_ADMISSION} remote connections are waiting to present a secret; \
                     closing the ones that arrive until some of them finish"
                );
            }
            drop(sock);
            continue;
        };
        let tls = Arc::clone(tls);
        let admissions = admissions.clone();
        let _ = std::thread::Builder::new()
            .name("koshi-remote".to_string())
            .spawn(move || serve_remote(sock, &tls, &admissions, counted));
    }
}

/// One connection inside the admission window, counted while it is there.
///
/// The count drops when this is dropped, whichever way the connection left:
/// admitted, refused, timed out, or hung up.
struct InAdmission {
    /// The shared count of connections inside the window.
    counted: Arc<AtomicUsize>,
}

impl InAdmission {
    /// Count one more connection, or `None` when [`MAX_IN_ADMISSION`] are
    /// already inside the window.
    fn enter(counted: &Arc<AtomicUsize>) -> Option<InAdmission> {
        let taken = counted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| {
                (now < MAX_IN_ADMISSION).then_some(now + 1)
            })
            .is_ok();
        taken.then(|| InAdmission {
            counted: Arc::clone(counted),
        })
    }
}

impl Drop for InAdmission {
    fn drop(&mut self) {
        self.counted.fetch_sub(1, Ordering::AcqRel);
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
struct Window {
    /// How many connections that address has opened since `opened`.
    attempts: u32,
    /// When the first of them arrived.
    opened: Instant,
}

/// How many connections each address has opened lately.
///
/// Bounded at [`MAX_ENTRIES`]. Every check first drops the addresses whose
/// window has passed; a check that still finds the table full drops the address
/// whose window opened first.
struct RateTable {
    /// One window per address.
    entries: HashMap<IpAddr, Window>,
}

impl RateTable {
    /// An empty table.
    fn new() -> RateTable {
        RateTable {
            entries: HashMap::new(),
        }
    }

    /// Count one connection from `ip` at `now` and say what to do with it.
    ///
    /// An address is logged once per window, on the attempt that crosses
    /// [`MAX_ATTEMPTS`]. Every later attempt in that window is dropped in
    /// silence.
    ///
    /// Example — with [`MAX_ATTEMPTS`] at 10, attempts 1 to 10 from one
    /// address are [`Attempt::Serve`], attempt 11 is [`Attempt::DropAndSay`],
    /// and attempts 12 onward are [`Attempt::DropInSilence`] until the window
    /// passes.
    fn allow(&mut self, ip: IpAddr, now: Instant) -> Attempt {
        self.entries
            .retain(|_, window| now.duration_since(window.opened) < RATE_WINDOW);
        if self.entries.len() >= MAX_ENTRIES && !self.entries.contains_key(&ip) {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, window)| window.opened)
                .map(|(address, _)| *address);
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        let window = self.entries.entry(ip).or_insert(Window {
            attempts: 0,
            opened: now,
        });
        window.attempts += 1;
        match window.attempts {
            counted if counted <= MAX_ATTEMPTS => Attempt::Serve,
            counted if counted == MAX_ATTEMPTS + 1 => Attempt::DropAndSay,
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
/// [`ADMISSION_WINDOW`], counted from the moment this thread starts. A refusal
/// written by [`refuse`] gets [`REFUSAL_WINDOW`] instead. Once the caller is
/// admitted both halves and the socket lose their deadlines and block for as
/// long as it takes.
///
/// Admission registers the connection with the router, attached or not. The
/// registration is dropped when the connection finishes, whichever step it
/// finished at.
///
/// `counted` holds this connection's place in the admission window and is
/// dropped the moment the secret is admitted.
///
/// On Unix the thread blocks SIGPIPE on its own signal mask; a write to a peer
/// that hung up returns an error whatever the process-wide disposition is.
fn serve_remote(
    mut sock: TcpStream,
    tls: &Arc<ServerConfig>,
    admissions: &Sender<RouterEvent>,
    counted: InAdmission,
) {
    #[cfg(unix)]
    crate::process::block_sigpipe_on_this_thread();

    let deadline = Instant::now() + ADMISSION_WINDOW;
    let Ok(control) = sock.try_clone() else {
        return;
    };
    let Ok(server) = ServerConnection::new(Arc::clone(tls)) else {
        return;
    };
    let mut conn = rustls::Connection::Server(server);
    if tls::handshake(&mut conn, &mut sock, deadline).is_err() {
        return;
    }
    let Ok((mut reader, mut writer)) = tls::split_tls(conn, sock) else {
        return;
    };

    reader.set_deadline(Some(deadline));
    writer.set_deadline(Some(deadline));
    let ((min_remote, max_remote), versions, token) =
        match read_client_frame(&mut reader, REMOTE_HELLO_MAX_LEN) {
            Opening::Frame(RemoteClientFrame::Hello {
                min_remote_version,
                max_remote_version,
                min_protocol_version,
                max_protocol_version,
                token,
            }) => (
                (min_remote_version, max_remote_version),
                (min_protocol_version, max_protocol_version),
                token,
            ),
            Opening::Frame(_) | Opening::Unreadable => {
                refuse_by(&mut writer, refusal_deadline(deadline));
                return;
            }
            Opening::Closed => return,
        };

    // The version is settled before the secret is looked at. This refusal names
    // both ranges instead of carrying REMOTE_REFUSED.
    let Some(remote_version) = agreed_version(
        min_remote,
        max_remote,
        MIN_REMOTE_PROTOCOL_VERSION,
        REMOTE_PROTOCOL_VERSION,
    ) else {
        let _ = send_frame(
            &mut writer,
            &RemoteServerFrame::Refused {
                message: version_refusal(min_remote, max_remote),
            },
        );
        return;
    };

    let Ok(registered) = control.try_clone() else {
        return;
    };
    let admitted = ask(admissions, |reply| AdmissionAsk::Admit {
        token,
        stream: registered,
        reply,
    });
    let Some(Some(admitted)) = admitted else {
        refuse_by(&mut writer, refusal_deadline(deadline));
        return;
    };

    // The caller leaves the admission window.
    drop(counted);

    // Both halves and the socket lose their deadlines.
    reader.set_deadline(None);
    writer.set_deadline(None);
    let _ = control.set_read_timeout(None);
    let _ = control.set_write_timeout(None);
    if send_frame(&mut writer, &RemoteServerFrame::Welcome { remote_version }).is_err() {
        report_ended(admissions, admitted.id);
        return;
    }

    serve_admitted(reader, writer, control, admitted, versions, admissions);
}

/// Serve an admitted connection: list the sessions its secret reaches, as
/// often as it asks, attach to one when it asks for that, and report the
/// connection ended when no bridge took it over.
///
/// `versions` is the session protocol range the client named in its opening
/// frame. Nothing here reads it: it is carried to
/// [`bridge_to_session`], which puts it in the session-plane Hello it sends
/// for this client, so the client and the session server settle a version
/// between themselves.
fn serve_admitted(
    mut reader: TlsReader,
    mut writer: TlsWriter,
    control: TcpStream,
    admitted: Admitted,
    versions: (u32, u32),
    admissions: &Sender<RouterEvent>,
) {
    let attached = admitted_frames(&mut reader, &mut writer, &admitted, admissions);
    match attached {
        Some(endpoint) => bridge_to_session(
            reader,
            writer,
            control,
            endpoint,
            admitted.id,
            versions,
            admissions,
        ),
        None => report_ended(admissions, admitted.id),
    }
}

/// Read the frames an admitted connection sends until it attaches or ends.
///
/// A list is answered and the next frame is read, so one connection may list
/// and then attach. `Some` is the endpoint file of the session an admitted
/// attach reached; the bytes after that attach belong to that session's server.
/// `None` means the connection is finished: it hung up, it sent something this
/// loop does not serve, its attach was refused, or the dispatcher is gone.
fn admitted_frames(
    reader: &mut impl Read,
    writer: &mut (impl Write + Deadlined),
    admitted: &Admitted,
    admissions: &Sender<RouterEvent>,
) -> Option<PathBuf> {
    loop {
        let frame = match read_client_frame(reader, MAX_FRAME_LEN) {
            Opening::Frame(frame) => frame,
            Opening::Unreadable => {
                refuse(writer);
                return None;
            }
            Opening::Closed => return None,
        };
        match frame {
            RemoteClientFrame::List => {
                let scope = admitted.scope.clone();
                let rows = ask(admissions, |reply| AdmissionAsk::Rows { scope, reply })?;
                if send_frame(writer, &RemoteServerFrame::Sessions { rows }).is_err() {
                    return None;
                }
            }
            RemoteClientFrame::Attach { session } => {
                let scope = admitted.scope.clone();
                let id = admitted.id;
                let located = ask(admissions, |reply| AdmissionAsk::Locate {
                    scope,
                    id,
                    selector: session,
                    reply,
                })?;
                let Some(endpoint) = located else {
                    refuse(writer);
                    return None;
                };
                return Some(endpoint);
            }
            RemoteClientFrame::Hello { .. } => {
                refuse(writer);
                return None;
            }
        }
    }
}

/// The Hello the router sends a session server for a caller this listener
/// accepted: `token` from that session's endpoint file, the caller's own
/// version range in `versions` as `(min, max)`, and `remote` set.
///
/// This is the only place `remote` is set.
fn bridged_hello(token: ConnectionToken, versions: (u32, u32)) -> IpcRequest {
    let (min_protocol_version, max_protocol_version) = versions;
    IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version,
            max_protocol_version,
            token,
            remote: true,
        },
    }
}

/// Open the local connection to the session advertised at `endpoint_path` and
/// send it the Hello carrying that session's endpoint token and `versions`.
///
/// Hands back the connection's two raw halves and the handle that closes its
/// read direction.
///
/// `None` when the endpoint file cannot be read, its socket cannot be reached,
/// the read direction cannot be made closable, or the Hello cannot be sent.
fn open_session_bridge(
    endpoint_path: &Path,
    versions: (u32, u32),
) -> Option<(RawReader, RawWriter, ReadCloser)> {
    let endpoint = EndpointFile::read(endpoint_path).ok()?;
    let mut local = Connection::connect(&endpoint.socket).ok()?;
    let closer = local.read_closer().ok()?;
    local.send(&bridged_hello(endpoint.token, versions)).ok()?;
    let (from_session, to_session) = local.split_raw();
    Some((from_session, to_session, closer))
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
fn bridge_to_session(
    reader: TlsReader,
    mut writer: TlsWriter,
    control: TcpStream,
    endpoint_path: PathBuf,
    id: u64,
    versions: (u32, u32),
    admissions: &Sender<RouterEvent>,
) {
    let Some((mut from_session, mut to_session, closer)) =
        open_session_bridge(&endpoint_path, versions)
    else {
        refuse(&mut writer);
        report_ended(admissions, id);
        return;
    };
    let ended = Arc::new(EndReport::new(admissions.clone(), id));

    let Ok(inbound_control) = control.try_clone() else {
        ended.once();
        return;
    };
    let mut inbound = reader;
    let inbound_ended = Arc::clone(&ended);
    let started = std::thread::Builder::new()
        .name("koshi-remote-in".to_string())
        .spawn(move || {
            #[cfg(unix)]
            crate::process::block_sigpipe_on_this_thread();
            let _ = io::copy(&mut inbound, &mut to_session);
            closer.close();
            let _ = inbound_control.shutdown(Shutdown::Both);
            inbound_ended.once();
        });
    if started.is_err() {
        ended.once();
        return;
    }

    let mut outbound = writer;
    // This handle stays here; the thread takes its own clone.
    let Ok(outbound_control) = control.try_clone() else {
        let _ = control.shutdown(Shutdown::Both);
        ended.once();
        return;
    };
    let outbound_ended = Arc::clone(&ended);
    let started = std::thread::Builder::new()
        .name("koshi-remote-out".to_string())
        .spawn(move || {
            #[cfg(unix)]
            crate::process::block_sigpipe_on_this_thread();
            let _ = io::copy(&mut from_session, &mut outbound);
            let _ = outbound_control.shutdown(Shutdown::Both);
            outbound_ended.once();
        });
    if started.is_err() {
        // Shutting the socket ends the inbound direction, which is already
        // running.
        let _ = control.shutdown(Shutdown::Both);
        ended.once();
    }
}

/// Report that one admitted connection has ended. It leaves the router's list
/// of live remote connections.
fn report_ended(admissions: &Sender<RouterEvent>, id: u64) {
    let _ = admissions.send(RouterEvent::Admission(AdmissionAsk::Ended { id }));
}

/// Reports one bridged connection ended. The first [`EndReport::once`] sends;
/// every later one does nothing.
struct EndReport {
    /// Where the report goes.
    admissions: Sender<RouterEvent>,
    /// The number the connection is registered under.
    id: u64,
    /// Set by the first report. Every later one does nothing.
    reported: std::sync::atomic::AtomicBool,
}

impl EndReport {
    /// A report for the connection registered under `id`, not yet made.
    fn new(admissions: Sender<RouterEvent>, id: u64) -> EndReport {
        EndReport {
            admissions,
            id,
            reported: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Report the connection ended, unless something already has.
    fn once(&self) {
        if self.reported.swap(true, Ordering::AcqRel) {
            return;
        }
        report_ended(&self.admissions, self.id);
    }
}

/// Put one question on the dispatcher's queue and wait for its answer.
///
/// `None` when the dispatcher is gone or hung up without answering.
fn ask<T>(
    admissions: &Sender<RouterEvent>,
    build: impl FnOnce(Sender<T>) -> AdmissionAsk,
) -> Option<T> {
    let (reply, answer) = mpsc::channel();
    admissions.send(RouterEvent::Admission(build(reply))).ok()?;
    answer.recv().ok()
}

/// Write one [`REMOTE_REFUSED`] frame, giving the write until `until`.
///
/// A write that fails is dropped, and so is one whose `until` has already
/// passed.
fn refuse_by(writer: &mut (impl Write + Deadlined), until: Instant) {
    writer.set_deadline(Some(until));
    let _ = send_frame(
        writer,
        &RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        },
    );
}

/// Write one [`REMOTE_REFUSED`] frame, replacing whatever deadline `writer`
/// holds with [`REFUSAL_WINDOW`] counted from now.
///
/// For a caller that is already admitted, whose halves carry no deadline. A
/// caller still inside the admission window is refused with [`refuse_by`],
/// which cannot hold a thread past that window.
fn refuse(writer: &mut (impl Write + Deadlined)) {
    refuse_by(writer, Instant::now() + REFUSAL_WINDOW);
}

/// Read one frame: a 4-byte big-endian length, then that many bytes of JSON.
///
/// The length is checked against `max_len` before the payload buffer is
/// allocated. Callers pass [`REMOTE_HELLO_MAX_LEN`] before admission and
/// [`MAX_FRAME_LEN`] after it. A length over `max_len` is [`Opening::Closed`]
/// and reads no payload.
fn read_client_frame<R: Read>(reader: &mut R, max_len: u32) -> Opening {
    let mut length_bytes = [0u8; 4];
    if reader.read_exact(&mut length_bytes).is_err() {
        return Opening::Closed;
    }
    let payload_len = u32::from_be_bytes(length_bytes);
    if payload_len > max_len {
        return Opening::Closed;
    }
    let mut payload = vec![0u8; payload_len as usize];
    if reader.read_exact(&mut payload).is_err() {
        return Opening::Closed;
    }
    match serde_json::from_slice(&payload) {
        Ok(frame) => Opening::Frame(frame),
        Err(_) => Opening::Unreadable,
    }
}

/// Write one frame: a 4-byte big-endian length, then the JSON, in one write.
///
/// # Errors
/// The JSON encoder's own failure, `the answer is larger than a frame can
/// carry` for a payload past `u32::MAX` bytes, and whatever the writer reports.
fn send_frame<W: Write>(writer: &mut W, frame: &RemoteServerFrame) -> io::Result<()> {
    let payload = serde_json::to_vec(frame)?;
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::other("the answer is larger than a frame can carry"))?;
    let mut bytes = Vec::with_capacity(payload.len() + 4);
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(&payload);
    writer.write_all(&bytes)
}

#[cfg(test)]
mod tests;
