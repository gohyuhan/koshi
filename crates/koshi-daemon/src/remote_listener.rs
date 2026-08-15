//! The TLS port this machine serves remote clients on.
//!
//! One port per machine, opened by the router. A remote client opens a TLS
//! stream, presents the secret from a grant, and then either lists the
//! sessions that secret reaches or attaches to one. The router keeps the
//! connection: it opens its own local connection to the session server,
//! presents that session's endpoint token on the client's behalf, and carries
//! the bytes both ways without reading them.
//!
//! Admission runs on the router's dispatcher, which owns the token store and
//! the session list. Carrying a connection's traffic never reaches the
//! dispatcher, so one remote client's typing cannot hold up a local koshi
//! command.
//!
//! A secret that is admitted registers the connection with the router, and it
//! stays registered until this listener reports it ended. So a revoke ends the
//! connection at once, whether it has attached to a session or is still
//! sitting on the Welcome.
//!
//! Every blocking step before the Welcome shares one deadline,
//! [`ADMISSION_WINDOW`]. Each single read and write inside those steps is
//! given the time left on that deadline when it starts, so the total an
//! unauthenticated caller can hold a connection thread is bounded whatever
//! pace it sends its bytes at.
//!
//! Every refusal is [`REMOTE_REFUSED`] and closes the connection. A wrong
//! secret, a revoked secret, a session that does not exist, and a session the
//! secret holds no grant for read the same and cost the same work: no name a
//! caller sent reaches a socket connect, a wait, or a file until the admitted
//! scope has been proven to cover it.
//!
//! `koshi share` needs nothing here. This listener carries the three remote
//! frames and then one session server's own bytes; no path from it reaches
//! the router's control plane, which is where share is answered. Whether a
//! client is local or remote is decided by which listener accepted it, never
//! by anything the client says about itself. The matching check inside a pane
//! is deliberately not built: a remote guest's pane shell is a local process,
//! and every check available there is defeated by unsetting `KOSHI_CLIENT_ID`
//! or by typing in a pane the owner opened. That is a guardrail, not a
//! boundary.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
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
use koshi_ipc::transport::{Connection, MAX_FRAME_LEN};

use crate::router::RouterEvent;

/// The total time a caller that has not been admitted may hold a connection
/// thread: the TLS handshake, the frame it opens with, and any refusal
/// written back all finish inside it.
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
/// The attempt limit is counted per address, so a flood from many addresses
/// slips past it and costs one thread each. This bounds that: a connection is
/// counted from the moment it is accepted until its secret is admitted or it
/// goes away, and one arriving over the bound is closed without a handshake.
/// An admitted connection is not counted, so a flood cannot shut out the
/// clients already attached, and the grants the operator issued are what
/// bounds those.
const MAX_IN_ADMISSION: usize = 64;

/// How long the accept loop pauses after a failed accept before trying again,
/// so a persistent accept error cannot spin a core.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// One thing the listener needs the router's dispatcher to decide, since the
/// dispatcher owns the token store and the session list.
pub(crate) enum AdmissionAsk {
    /// What a presented secret reaches. A secret that reaches something
    /// registers this connection, so a revoke can end it from here on.
    Admit {
        /// The secret the caller presented.
        token: ConnectionToken,
        /// The connection's socket, kept so a revoke can end it.
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
    /// One admitted connection has ended, so it leaves the router's list.
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

/// Open the TLS port at `address`, presenting `cert`.
///
/// Binding happens here, so the caller learns of an address it cannot bind and
/// carries on: a remote setting that does not work never stops koshi and never
/// stops local clients. With the port bound, one thread accepts connections and
/// gives each its own thread, and `admissions` carries every question those
/// threads need the router's dispatcher to answer.
///
/// # Errors
/// The certificate that could not be turned into a TLS configuration, or the
/// address that could not be bound.
pub(crate) fn start(
    address: String,
    cert: CertFile,
    admissions: Sender<RouterEvent>,
) -> io::Result<()> {
    let tls = Arc::new(server_config(&cert)?);
    let listener = TcpListener::bind(&address)?;
    std::thread::Builder::new()
        .name("koshi-remote-accept".to_string())
        .spawn(move || accept_loop(&listener, &tls, &admissions))?;
    Ok(())
}

/// The TLS configuration this machine serves with: its own certificate, its
/// private key, and no client certificate asked for.
///
/// Identity is the secret a caller presents in its opening frame, so the
/// handshake asks the caller for nothing.
fn server_config(cert: &CertFile) -> io::Result<ServerConfig> {
    let chain = vec![CertificateDer::from(cert.cert_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_der.clone()));
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|error| io::Error::other(error.to_string()))
}

/// Accept connections and give each its own thread, dropping the ones from an
/// address that has opened too many. A failed accept pauses briefly and
/// retries.
fn accept_loop(listener: &TcpListener, tls: &Arc<ServerConfig>, admissions: &Sender<RouterEvent>) {
    let mut attempts = RateTable::new();
    let in_admission = Arc::new(AtomicUsize::new(0));
    let mut said_full = false;
    loop {
        let (sock, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(_) => {
                std::thread::sleep(ACCEPT_RETRY_DELAY);
                continue;
            }
        };
        let ip = peer.ip();
        match attempts.allow(ip, Instant::now()) {
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
            if !said_full {
                said_full = true;
                tracing::warn!(
                    "{MAX_IN_ADMISSION} remote connections are waiting to present a secret; \
                     closing the ones that arrive until some of them finish"
                );
            }
            drop(sock);
            continue;
        };
        said_full = false;
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
    /// same window, and one line per attempt would be the flood the limit
    /// exists to keep out of the log.
    DropInSilence,
}

/// What one address has done inside the window it opened.
struct Window {
    /// How many connections that address has opened since `opened`.
    attempts: u32,
    /// When the first of them arrived.
    opened: Instant,
}

/// How many connections each address has opened lately, so a flood is a log
/// line rather than a flood.
///
/// The table is bounded at [`MAX_ENTRIES`]. Every check first drops the
/// addresses whose window has passed; a check that still finds the table full
/// drops the address whose window opened first. So the table cannot grow past
/// that count however many addresses connect.
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
    /// [`MAX_ATTEMPTS`] — every later attempt in that window is dropped in
    /// silence, so a caller hammering the port cannot write the log full.
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
/// Every step up to the Welcome finishes inside [`ADMISSION_WINDOW`], counted
/// from the moment this thread starts. Once the caller is admitted the socket
/// timeouts are cleared, since an admitted client may sit as long as it likes.
///
/// Admission registers the connection with the router, so a revoke ends it
/// whether it has attached or not. The registration is dropped when the
/// connection finishes, whichever step it finished at.
///
/// `counted` holds this connection's place in the admission window. It is
/// dropped the moment the secret is admitted, so an attached client never
/// counts against the callers still waiting to present one.
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
    crate::router::block_sigpipe_on_this_thread();

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
    let opening = match read_client_frame(&mut reader, REMOTE_HELLO_MAX_LEN) {
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
            refuse(&mut writer);
            return;
        }
        Opening::Closed => return,
    };
    let ((min_remote, max_remote), versions, token) = opening;

    // The version is settled before the secret is looked at, the same order the
    // local planes use. A refusal here names both ranges: it says nothing about
    // secrets or sessions, and a caller told only the uniform sentence would
    // have no way to learn which end to upgrade.
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
        refuse(&mut writer);
        return;
    };

    // The caller is admitted, so it leaves the admission window and stops
    // counting against the connections waiting to present a secret.
    drop(counted);

    // Nothing it does from here is on a clock either: the halves stop setting
    // the timeouts, and the last ones they set are cleared.
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
/// attach reached, and the bytes after that attach belong to that session's
/// server. `None` means the connection is finished: it hung up, it sent
/// something this loop does not serve, or its attach was refused.
fn admitted_frames(
    reader: &mut TlsReader,
    writer: &mut TlsWriter,
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

/// Open the local connection to an admitted client's session and carry the
/// bytes both ways.
///
/// The router presents the session's endpoint token on the client's behalf,
/// since a caller on another machine cannot read that file. The session
/// server's answer to that Hello, and everything after it, travels back
/// through the bridge unread.
///
/// Two threads carry the two directions. Whichever ends first shuts the TCP
/// socket down in both directions, which ends the thread reading the TLS
/// stream at once, and closes the local connection's read direction, which on
/// Unix ends the thread reading the session at once. A Windows named pipe
/// carries no read direction to shut on its own, so there that thread ends at
/// the session server's next message or when it hangs up.
fn bridge_to_session(
    reader: TlsReader,
    mut writer: TlsWriter,
    control: TcpStream,
    endpoint_path: PathBuf,
    id: u64,
    versions: (u32, u32),
    admissions: &Sender<RouterEvent>,
) {
    let Ok(endpoint) = EndpointFile::read(&endpoint_path) else {
        refuse(&mut writer);
        report_ended(admissions, id);
        return;
    };
    let Ok(mut local) = Connection::connect(&endpoint.socket) else {
        refuse(&mut writer);
        report_ended(admissions, id);
        return;
    };
    let Ok(closer) = local.read_closer() else {
        refuse(&mut writer);
        report_ended(admissions, id);
        return;
    };
    let (min_protocol_version, max_protocol_version) = versions;
    let hello = IpcRequest {
        request_id: 1,
        kind: IpcRequestKind::Hello {
            min_protocol_version,
            max_protocol_version,
            token: endpoint.token,
        },
    };
    if local.send(&hello).is_err() {
        refuse(&mut writer);
        report_ended(admissions, id);
        return;
    }
    let (mut from_session, mut to_session) = local.split_raw();

    let Ok(inbound_control) = control.try_clone() else {
        report_ended(admissions, id);
        return;
    };
    let mut inbound = reader;
    let started = std::thread::Builder::new()
        .name("koshi-remote-in".to_string())
        .spawn(move || {
            #[cfg(unix)]
            crate::router::block_sigpipe_on_this_thread();
            let _ = io::copy(&mut inbound, &mut to_session);
            closer.close();
            let _ = inbound_control.shutdown(Shutdown::Both);
        });
    if started.is_err() {
        report_ended(admissions, id);
        return;
    }

    let mut outbound = writer;
    let reporter = admissions.clone();
    // The thread gets its own handle and this one stays here, so a thread that
    // does not start can still end the direction that did.
    let Ok(outbound_control) = control.try_clone() else {
        let _ = control.shutdown(Shutdown::Both);
        report_ended(admissions, id);
        return;
    };
    let started = std::thread::Builder::new()
        .name("koshi-remote-out".to_string())
        .spawn(move || {
            #[cfg(unix)]
            crate::router::block_sigpipe_on_this_thread();
            let _ = io::copy(&mut from_session, &mut outbound);
            let _ = outbound_control.shutdown(Shutdown::Both);
            report_ended(&reporter, id);
        });
    if started.is_err() {
        // The other direction is already carrying this client's bytes into the
        // session. Shutting the socket ends it, so the connection cannot
        // outlive the registration a revoke cuts it by.
        let _ = control.shutdown(Shutdown::Both);
        report_ended(admissions, id);
    }
}

/// Report that one admitted connection has ended, so it leaves the router's
/// list of live remote connections.
fn report_ended(admissions: &Sender<RouterEvent>, id: u64) {
    let _ = admissions.send(RouterEvent::Admission(AdmissionAsk::Ended { id }));
}

/// Put one question on the dispatcher's queue and wait for its answer.
///
/// `None` means the dispatcher is gone — the router is exiting — so the caller
/// closes its connection.
fn ask<T>(
    admissions: &Sender<RouterEvent>,
    build: impl FnOnce(Sender<T>) -> AdmissionAsk,
) -> Option<T> {
    let (reply, answer) = mpsc::channel();
    admissions.send(RouterEvent::Admission(build(reply))).ok()?;
    answer.recv().ok()
}

/// Answer one refusal, in the one sentence every refusal carries.
///
/// The writing half holds the deadline while the caller has not been admitted,
/// so the write ends inside [`ADMISSION_WINDOW`] however slowly the caller
/// takes the bytes. A write that fails changes nothing: the caller closes the
/// connection either way.
fn refuse(writer: &mut TlsWriter) {
    let _ = send_frame(
        writer,
        &RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        },
    );
}

/// Read one frame: a 4-byte big-endian length, then that many bytes of JSON.
///
/// The length is checked against `max_len` before the payload buffer is
/// allocated, so a caller naming a huge length is dropped at the cost of
/// reading four bytes. Before the caller is admitted `max_len` is
/// [`REMOTE_HELLO_MAX_LEN`]; after it, the frame cap the rest of koshi uses.
fn read_client_frame<R: Read>(reader: &mut R, max_len: u32) -> Opening {
    let mut length = [0u8; 4];
    if reader.read_exact(&mut length).is_err() {
        return Opening::Closed;
    }
    let len = u32::from_be_bytes(length);
    if len > max_len {
        return Opening::Closed;
    }
    let mut payload = vec![0u8; len as usize];
    if reader.read_exact(&mut payload).is_err() {
        return Opening::Closed;
    }
    match serde_json::from_slice(&payload) {
        Ok(frame) => Opening::Frame(frame),
        Err(_) => Opening::Unreadable,
    }
}

/// Write one frame: a 4-byte big-endian length, then the JSON, in one write.
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
