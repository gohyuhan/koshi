//! The control-socket server: how a running koshi answers its socket.
//!
//! [`IpcServer::start`] binds the session's control-socket address, writes
//! the endpoint file advertising it, and spawns the accept loop. Each
//! accepted connection gets its own thread holding its own
//! [`Handshake`] gate: a Hello must open the connection before any other
//! request is served. A `SubmitCommand`, `Discovery`, `Layout`, `Attach` or
//! `Restart` request crosses to the dispatcher thread through the runtime inbox
//! with a reply channel; the dispatcher's answer comes back on it and leaves as
//! the connection's response frame.
//!
//! A `SubmitCommand`'s [`CommandSource`] is set here, from the connection the
//! request arrived on, over whatever source the peer wrote: a control
//! connection carries a CLI source, and an attached client's connection
//! carries [`CommandSource::KeyBinding`] naming that connection's own client.
//!
//! An `Attach` is the one request that keeps its connection: once the reply
//! carrying the session's structure is written, the connection is split. The
//! writing half carries that client's event stream, and the reading half
//! carries that client's key presses, resizes, pasted text and commands to the
//! dispatcher and writes nothing back. The peer going away detaches the client.
//!
//! Passing [`OtherUsers`] to [`IpcServer::start`] moves the control socket to
//! the machine-wide shared directory and widens it, so the other local users
//! of this machine can reach it. The endpoint file keeps its private place and
//! its `0600` mode either way. Every accepted connection is gated by the user
//! the OS reports for it: the user who started the session is always served,
//! and another local user is served only while `allow-other-users` is on. The
//! setting is read again for each of that user's requests, so turning it off
//! closes their connections.
//!
//! The decisions every koshi server makes the same way — a request kind this
//! build does not have, a malformed-but-aligned frame, an oversize frame, and
//! the Hello — belong to [`plane::next_request`], which this loop reads its
//! requests through. None of them reaches the session, any pane, or any other
//! connection.
//!
//! A `Leaving` request ends the connection it arrives on: the thread serving it
//! stops reading and the connection closes. Requests arrive in the order the
//! peer queued them, so every request that peer sent is already with the
//! dispatcher by then. [`IpcServer::attached_connections`] counts the attached
//! clients still being read.
//!
//! [`IpcServer::close_intake`] ends the connections that are left: no event a
//! peer sends from here is handed to the dispatcher, and every connection being
//! served has its read direction closed, so its thread ends without reading
//! another request. The dispatcher calls it before it stops draining the inbox.
//!
//! [`IpcServer::shutdown`] stops accepting, joins the accept loop, and
//! removes the endpoint file, the socket and any shared marker, so nothing
//! advertises a session that is gone.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use koshi_core::command::{CommandEnvelope, CommandSource};
use koshi_core::ids::{ClientId, PaneId, SessionId};
use koshi_ipc::endpoint::{
    advert_path, remove_advert, remove_socket_file, shared_socket_addr, socket_addr, write_advert,
    EndpointFile,
};
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::{FrameImageChunk, PaintedFrame, MAX_FRAME_IMAGE_TRANSFERS};
use koshi_ipc::handshake::{Handshake, Peer};
use koshi_ipc::plane::{self, Next};
use koshi_ipc::protocol::{
    ConnectionToken, GraphicsCapabilities, IncomingRequest, IpcErrorCode, IpcErrorPayload,
    IpcRequestKind, IpcResponse, IpcResult, SessionPlane,
};
use koshi_ipc::transport::{self, Connection, FrameWriter, Listener, ReadCloser};
use koshi_ipc::validate::{
    reclaim_stale_socket, validate_shared_socket_addr, validate_socket_addr,
};
use koshi_ipc::wire::MaybeKnown;
use koshi_observability::logging::recent_events;
use koshi_renderer::snapshot::{Delivery, ImagePlacementSnapshot, RenderSnapshot};
use koshi_terminal::graphics::ImageRecord;

use crate::runtime::bus::wire_event;
use crate::runtime::event::{EndingNotice, RuntimeEvent, SessionEnding};
use crate::runtime::frame::{
    wire_frame, wire_frame_with_content_ids, wire_image_chunk_sources, wire_image_transfer,
};

/// How long the accept loop sleeps after a failed accept before it accepts
/// again. A persistent accept error — say, the process is out of file
/// descriptors — retries once per interval.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The version of the binary this session server is, reported in its Hello
/// answer.
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Reads the `allow-other-users` setting from the configuration again and
/// reports whether it is on. Called for each connection from another local
/// user and for each request that user sends, so the answer is always the
/// current one.
pub type OtherUsersSetting = Arc<dyn Fn() -> bool + Send + Sync>;

/// What [`IpcServer::start`] needs to serve the other local users of this
/// machine.
///
/// Only the control socket moves: it binds in this user's directory under the
/// machine-wide shared directory, carrying mode `0666` on Unix and a security
/// descriptor granting Authenticated Users read and write on Windows. The
/// endpoint file stays in the private runtime directory at mode `0600`, so the
/// token it carries stays readable only by the user who started the session.
pub struct OtherUsers {
    /// The machine-wide directory koshi shares between local users, from
    /// `koshi_paths::shared_sessions_dir`. This user's directory inside it
    /// holds the control socket.
    pub shared_dir: PathBuf,
    /// The live read of the `allow-other-users` setting.
    pub still_on: OtherUsersSetting,
}

/// What the control socket takes in: every connection being served, how many of
/// them carry an attached client, and whether what a peer sends still reaches
/// the session.
///
/// A serving thread reads `closed` and hands its event to the dispatcher under
/// one shared borrow of the state, and [`close`](Intake::close) sets `closed`
/// under the exclusive borrow. So a hand-off either finished before the close,
/// or reads the closed intake and never happens: once
/// [`close`](Intake::close) returns, no serving thread sends another event.
#[derive(Debug, Default)]
struct Intake {
    state: RwLock<IntakeState>,
}

/// What an [`Intake`] holds.
#[derive(Debug, Default)]
struct IntakeState {
    /// `true` once [`Intake::close`] ran. No event reaches the dispatcher from
    /// here.
    closed: bool,
    /// How many connections carry an attached client and are still being read.
    attached: usize,
    /// The read direction of every connection being served, under the number
    /// the intake filed it as. The [`ServedConnection`] its thread holds
    /// removes the entry when that thread ends.
    readers: HashMap<u64, ReadCloser>,
    /// The number the next accepted connection is filed under.
    next: u64,
}

impl Intake {
    /// File `connection`'s read direction and hand back the entry its serving
    /// thread holds. `None` means the connection is not served: intake is
    /// closed, or its read direction could not be taken.
    fn accept(self: &Arc<Self>, connection: &Connection) -> Option<ServedConnection> {
        let reader = connection.read_closer().ok()?;
        let mut state = self.state.write().expect("intake");
        if state.closed {
            return None;
        }
        let number = state.next;
        state.next += 1;
        state.readers.insert(number, reader);
        Some(ServedConnection {
            intake: Arc::clone(self),
            number,
        })
    }

    /// Hand `event` to the dispatcher over the runtime inbox. `false` means it
    /// was not handed over — intake is closed, or the dispatcher is gone — and
    /// the caller's connection ends.
    fn hand_over(&self, inbox_tx: &Sender<RuntimeEvent>, event: RuntimeEvent) -> bool {
        let state = self.state.read().expect("intake");
        !state.closed && inbox_tx.send(event).is_ok()
    }

    /// Count one attached client's connection as being read, and hand back the
    /// entry the thread reading it holds.
    fn attached(self: &Arc<Self>) -> AttachedConnection {
        self.state.write().expect("intake").attached += 1;
        AttachedConnection {
            intake: Arc::clone(self),
        }
    }

    /// How many connections carry an attached client and are still being read.
    fn attached_connections(&self) -> usize {
        self.state.read().expect("intake").attached
    }

    /// Take connections again after a [`close`](Intake::close): a connection
    /// accepted from here on is served, and a serving thread hands its events
    /// over again.
    ///
    /// The connections closed by that call stay closed.
    fn reopen(&self) {
        self.state.write().expect("intake").closed = false;
    }

    /// Stop events reaching the dispatcher and close the read direction of
    /// every connection being served, so each thread ends without reading
    /// another request.
    fn close(&self) {
        let mut state = self.state.write().expect("intake");
        state.closed = true;
        for reader in state.readers.values() {
            reader.close();
        }
    }
}

/// One attached client's connection, counted in the [`Intake`] while the thread
/// reading that connection holds this. The count drops when that thread ends,
/// whether the client left, the connection broke, or the intake closed.
struct AttachedConnection {
    /// The intake this connection is counted in.
    intake: Arc<Intake>,
}

impl Drop for AttachedConnection {
    fn drop(&mut self) {
        self.intake.state.write().expect("intake").attached -= 1;
    }
}

/// One connection's entry in the [`Intake`], held by the thread serving that
/// connection. The entry is removed when that thread ends, so the intake holds
/// the read direction of the connections being served and no others.
struct ServedConnection {
    /// The intake this entry belongs to, and the gate the serving thread hands
    /// its events over through.
    intake: Arc<Intake>,
    /// The number this connection is filed under.
    number: u64,
}

/// The accepted client state consumed by one attached event stream.
struct AttachedStream {
    /// Client whose input and output use this stream.
    client_id: ClientId,
    /// Deliveries waiting to be written to the client.
    events: Receiver<Delivery>,
    /// Shared session-ending notice read by the writer.
    ending_notice: Arc<EndingNotice>,
    /// Graphics capabilities reported by this connection.
    graphics: GraphicsCapabilities,
}

impl Drop for ServedConnection {
    fn drop(&mut self) {
        self.intake
            .state
            .write()
            .expect("intake")
            .readers
            .remove(&self.number);
    }
}

/// The serving side of one session's control socket: the bound listener's
/// accept loop, the address it serves, and the endpoint file advertising it.
///
/// Held by the server for the session's lifetime; [`shutdown`](Self::shutdown)
/// stops the loop and withdraws the files it wrote.
#[derive(Debug)]
pub struct IpcServer {
    /// The control-socket address the accept loop is serving.
    addr: String,
    /// The endpoint file advertising `addr` and the connection token.
    endpoint_path: PathBuf,
    /// The empty marker naming this session among those other local users may
    /// reach, written on Windows where a pipe has no filesystem entry. `None`
    /// on Unix, and `None` for a session only its own user may reach.
    advert: Option<PathBuf>,
    /// Set by [`shutdown`](Self::shutdown); the accept loop exits when it
    /// observes the flag.
    shutting_down: Arc<AtomicBool>,
    /// The secret a connection presents at Hello, shared with the accept loop,
    /// which reads it for each connection it accepts.
    token: Arc<RwLock<ConnectionToken>>,
    /// What the socket takes in, shared with every serving thread.
    intake: Arc<Intake>,
    /// The accept loop, joined at shutdown. `None` once
    /// [`stop`](Self::stop) has taken it out to join it.
    accept_thread: Option<JoinHandle<()>>,
}

impl IpcServer {
    /// Bind `session`'s control socket, write the endpoint file advertising
    /// it, and start serving.
    ///
    /// The steps run in trust order: the runtime directory is created
    /// private (`0700`), the address is checked against the directory it sits
    /// in, any stale leftover socket is reclaimed, the listener binds, and
    /// only then is the endpoint file written — so the advertisement never
    /// exists without a listener behind it. A failed endpoint write unwinds
    /// the bind and leaves nothing behind.
    ///
    /// `other_users` `None` binds the socket inside `runtime_dir`, where only
    /// the user who started the session can reach it. `Some` binds it in this
    /// user's directory under the machine-wide shared directory instead and
    /// opens it to the other local users: mode `0666` on Unix, a pipe carrying
    /// the Authenticated Users access of [`Listener::bind_shared`] on Windows,
    /// where the marker naming the pipe is written as well. The endpoint file
    /// is written to the same private path at the same `0600` mode either way;
    /// only the address it carries differs.
    pub fn start(
        runtime_dir: &Path,
        session: SessionId,
        inbox_tx: Sender<RuntimeEvent>,
        other_users: Option<OtherUsers>,
    ) -> Result<IpcServer, IpcError> {
        koshi_paths::ensure_private_dir(runtime_dir).map_err(|error| IpcError::Transport {
            detail: format!(
                "could not create the runtime directory {}: {error}",
                runtime_dir.display()
            ),
        })?;
        let (addr, advert) = match &other_users {
            None => {
                let addr = socket_addr(runtime_dir, session);
                validate_socket_addr(&addr, runtime_dir)?;
                (addr, None)
            }
            Some(other_users) => {
                let shared_user_dir = ensure_shared_dirs(&other_users.shared_dir)?;
                let addr = shared_socket_addr(&shared_user_dir, session);
                validate_shared_socket_addr(&addr, &shared_user_dir)?;
                // A Windows pipe has no filesystem entry, so a marker file is
                // what names the session listening on one.
                let advert = cfg!(windows).then(|| advert_path(&shared_user_dir, session));
                (addr, advert)
            }
        };
        reclaim_stale_socket(&addr)?;
        let listener = if other_users.is_some() {
            Listener::bind_shared(&addr)?
        } else {
            Listener::bind(&addr)?
        };
        #[cfg(unix)]
        if other_users.is_some() {
            if let Err(error) = widen_socket(&addr) {
                drop(listener);
                remove_socket_file(&addr);
                return Err(error);
            }
        }

        let token = ConnectionToken::generate();
        let endpoint_path = EndpointFile::path(runtime_dir, session);
        let endpoint = EndpointFile {
            socket: addr.clone(),
            token: token.clone(),
            pid: std::process::id(),
        };
        let advertised = endpoint.write(&endpoint_path).and_then(|()| match &advert {
            None => Ok(()),
            Some(advert) => write_advert(advert),
        });
        if let Err(error) = advertised {
            // Dropping the listener releases the address and unlinks the socket
            // file on Unix, so a failed start leaves nothing behind. The
            // endpoint write is atomic, so a file left at `endpoint_path` is an
            // older run's, naming the socket removed here.
            let _ = std::fs::remove_file(&endpoint_path);
            drop(listener);
            remove_socket_file(&addr);
            return Err(error);
        }

        let still_on = other_users.map(|other_users| other_users.still_on);
        let shutting_down = Arc::new(AtomicBool::new(false));
        let accept_flag = Arc::clone(&shutting_down);
        let intake = Arc::new(Intake::default());
        let accept_intake = Arc::clone(&intake);
        let token = Arc::new(RwLock::new(token));
        let accept_token = Arc::clone(&token);
        let accept_thread = std::thread::spawn(move || {
            accept_loop(
                &listener,
                &accept_token,
                &inbox_tx,
                &accept_flag,
                still_on.as_ref(),
                &accept_intake,
            );
        });

        Ok(IpcServer {
            addr,
            endpoint_path,
            advert,
            shutting_down,
            token,
            intake,
            accept_thread: Some(accept_thread),
        })
    }

    /// End every connection still being served: no event a peer sends from here
    /// is handed to the dispatcher, and each connection has its read direction
    /// closed, so the thread serving it ends without reading another request.
    ///
    /// A request a peer sent before this either reached the dispatcher already
    /// or never does, so the dispatcher's next pass over the runtime inbox is
    /// the last that can find anything a peer sent.
    ///
    /// A peer still holding its connection loses what it wrote and the session
    /// had not read, and sees its connection close.
    ///
    /// A connection accepted after this is closed without being served.
    /// Closing an already-closed intake changes nothing.
    pub fn close_intake(&self) {
        self.intake.close();
    }

    /// Mint a fresh connection token, accept it from the next connection on,
    /// and rewrite the endpoint file with it.
    ///
    /// The address does not change. A connection already open keeps serving:
    /// the token is checked at Hello only. An intake closed by
    /// [`close_intake`](Self::close_intake) takes connections again, so a
    /// caller that closed it before this serves on the same socket afterwards.
    ///
    /// # Errors
    /// Returns the failure of writing the endpoint file. The fresh token is
    /// the one this server accepts either way, so the endpoint file then
    /// advertises a token no connection is served under.
    pub fn rotate_token(&self) -> Result<(), IpcError> {
        let fresh = ConnectionToken::generate();
        // Stored before the intake takes connections again, so no connection is
        // accepted under the token this replaces. Accepted before it is
        // advertised.
        *self.token.write().expect("token") = fresh.clone();
        self.intake.reopen();
        let endpoint = EndpointFile {
            socket: self.addr.clone(),
            token: fresh,
            pid: std::process::id(),
        };
        endpoint.write(&self.endpoint_path)
    }

    /// How many attached clients' connections are still being read.
    ///
    /// A client that read the session's `Restarting` frame sends `Leaving` and
    /// writes nothing after it, and its connection ends once the session has
    /// read everything it sent. After that frame, `0` means every attached
    /// client's input is in the runtime inbox.
    #[must_use]
    pub fn attached_connections(&self) -> usize {
        self.intake.attached_connections()
    }

    /// The control-socket address this server is serving.
    #[must_use]
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// Stop serving: no further connection is accepted, the accept loop is
    /// joined, and the endpoint file, the socket and any shared marker are
    /// removed. Connections already being served run out on their own threads;
    /// with the dispatcher draining, their in-flight requests end in a closed
    /// connection rather than a mutation.
    ///
    /// Dropping an `IpcServer` runs the same teardown, so a path that never
    /// reaches an explicit shutdown — a panic unwinding the server — still
    /// withdraws the files.
    pub fn shutdown(self) {
        drop(self);
    }

    /// The teardown itself, safe to run at most once per field: the join is
    /// guarded by taking `accept_thread`, and removing an already-removed
    /// file is a no-op.
    fn stop(&mut self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept_thread.take() {
            // The accept loop sits blocked in `accept`. A bare connect wakes it
            // and it reads the flag. That connection stays open across the join:
            // on Windows a connect that drops before `accept` runs can leave
            // nothing for `accept` to return. A failed connect — say, the
            // process is out of file descriptors — leaves the loop blocked and
            // skips the join; that thread ends with the process, and the files
            // below are removed either way.
            if let Ok(wake) = Connection::connect(&self.addr) {
                let _ = handle.join();
                drop(wake);
            }
        }
        let _ = std::fs::remove_file(&self.endpoint_path);
        if let Some(advert) = &self.advert {
            remove_advert(advert);
        }
        remove_socket_file(&self.addr);
    }
}

/// Create the machine-wide shared directory and this user's directory inside
/// it, and hand back this user's directory: where a session other local users
/// may reach binds its control socket.
fn ensure_shared_dirs(shared_dir: &Path) -> Result<PathBuf, IpcError> {
    koshi_paths::ensure_shared_base(shared_dir).map_err(|error| IpcError::Transport {
        detail: format!(
            "could not create the shared session directory {}: {error}",
            shared_dir.display()
        ),
    })?;
    koshi_paths::ensure_shared_user_dir(shared_dir).map_err(|error| IpcError::Transport {
        detail: format!(
            "could not create this user's directory under {}: {error}",
            shared_dir.display()
        ),
    })
}

/// Set the socket file at `addr` to mode `0666`, so every local user of this
/// machine may connect to it. Unix only: on Windows the address is a pipe name
/// with no filesystem entry.
#[cfg(unix)]
fn widen_socket(addr: &str) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(addr, std::fs::Permissions::from_mode(0o666)).map_err(|error| {
        IpcError::Transport {
            detail: format!("could not widen the control socket {addr}: {error}"),
        }
    })
}

/// Which peer a newly accepted connection is gated as. `same_user` is what the
/// OS reports about the process that connected, and `switch_on` is the
/// `allow-other-users` setting read a moment ago.
///
/// Another local user arriving while the setting is off is gated as a peer the
/// handshake refuses with `OtherUsersOff`, so no request of theirs is served.
fn admit(same_user: bool, switch_on: bool) -> Peer {
    Peer::Local {
        same_user,
        other_users_allowed: switch_on,
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Accept connections until the shutdown flag is set, giving each its own
/// serving thread. A failed accept pauses briefly and retries, so one
/// refused connection cannot stop the socket answering.
///
/// `still_on` is the live read of the `allow-other-users` setting, and is
/// `None` for a session only its own user may reach. Each connection is gated
/// by the user the OS reports for it; a connection whose user cannot be read
/// is closed without being served.
///
/// `intake` files each connection's read direction before its thread starts,
/// and is what that thread hands its events over through. A connection the
/// intake does not take — it is closed, or the read direction could not be
/// taken — is closed without being served.
fn accept_loop(
    listener: &Listener,
    token: &RwLock<ConnectionToken>,
    inbox_tx: &Sender<RuntimeEvent>,
    shutting_down: &AtomicBool,
    still_on: Option<&OtherUsersSetting>,
    intake: &Arc<Intake>,
) {
    transport::accept_until_shutdown(listener, shutting_down, ACCEPT_RETRY_DELAY, |connection| {
        // The OS reports which user opened the connection, so a peer
        // cannot claim to be another one.
        let Ok(same_user) = connection.peer_is_same_user() else {
            return;
        };
        let Some(served) = intake.accept(&connection) else {
            return;
        };
        let peer = admit(same_user, still_on.is_some_and(|still_on| still_on()));
        // Another local user's connection carries the setting with it,
        // so each of their requests is checked against it again.
        let live_setting = if same_user { None } else { still_on.cloned() };
        // Read for each connection, so a token rotated after this
        // server started is the one the next Hello is checked against.
        let token = token.read().expect("token").clone();
        let inbox_tx = inbox_tx.clone();
        std::thread::spawn(move || {
            serve_connection(connection, token, &inbox_tx, peer, live_setting, &served);
        });
    });
}

/// Serve one connection until its peer hangs up or a fault closes it.
///
/// [`plane::next_request`] makes every decision that is the same on every
/// koshi protocol — the framing faults, a request kind this build does not
/// have, and the Hello — and reads `live_setting` before any of its answers go
/// out. What is left is this session's own vocabulary: `SubmitCommand`,
/// `Attach`, `Discovery`, `Layout` and `Restart` cross to the dispatcher over
/// the inbox and answer with its reply. `RecentEvents` is answered on this
/// thread, from the process-wide log ring, and reaches no dispatcher.
///
/// A `SubmitCommand` on this connection has its source stamped by
/// [`cli_source`] before it crosses to the dispatcher: this connection carries
/// a `koshi` CLI invocation.
///
/// A `Restart` the dispatcher refuses is answered with
/// [`IpcErrorCode::MalformedRequest`] carrying the sentence naming what is
/// wrong, and the connection keeps serving.
///
/// An answered `Attach` ends the request loop: the connection is handed to
/// [`stream_events`], which carries that client's events out and that client's
/// input in for as long as the connection lives.
///
/// `live_setting` is the live read of the `allow-other-users` setting on a
/// connection from another local user, and `None` on one from the user who
/// started the session. It is read once a request has arrived and before any
/// answer goes out, so turning the setting off closes the connection with
/// nothing written.
///
/// A `Leaving` request ends the connection with no answer.
///
/// `served` is this connection's entry in the [`Intake`]: the intake holds this
/// connection's read direction through it, and the entry is removed when this
/// function returns. An intake that closes ends the connection, whatever the
/// request was.
fn serve_connection(
    mut connection: Connection,
    token: ConnectionToken,
    inbox_tx: &Sender<RuntimeEvent>,
    peer: Peer,
    live_setting: Option<OtherUsersSetting>,
    served: &ServedConnection,
) {
    let mut gate = Handshake::new(token, peer);
    // The setting can change while this connection is open, so it is read
    // again for each request. `None` is a connection from the user who started
    // the session, whose admission cannot be withdrawn.
    let admission = live_setting.clone();
    let admitted = move || match &admission {
        None => true,
        Some(still_on) => still_on(),
    };
    loop {
        let (request_id, kind) = match plane::next_request::<SessionPlane>(
            &mut connection,
            &mut gate,
            BUILD_VERSION,
            &admitted,
        ) {
            Next::Answered => continue,
            Next::Stop => return,
            Next::Dispatch { request_id, kind } => (Some(request_id), kind),
        };

        let response = match kind {
            // Answered before dispatch, so it never reaches this match.
            IpcRequestKind::Hello { .. } => {
                unreachable!("Hello is answered by the connection thread before dispatch")
            }
            IpcRequestKind::SubmitCommand(envelope) => {
                let envelope = cli_source(*envelope);
                let answer = ask_dispatcher(&served.intake, inbox_tx, |reply| RuntimeEvent::Ipc {
                    envelope,
                    reply,
                });
                let Some(result) = answer else {
                    return;
                };
                IpcResponse {
                    request_id,
                    result: IpcResult::CommandResult(result),
                }
            }
            IpcRequestKind::Attach {
                viewport,
                filter,
                resume,
                resume_token,
                pane_area,
                graphics,
            } => {
                let answer =
                    ask_dispatcher(&served.intake, inbox_tx, |reply| RuntimeEvent::IpcAttach {
                        resume,
                        resume_token,
                        viewport,
                        pane_area,
                        filter: filter.into(),
                        attached_at: SystemTime::now(),
                        remote: gate.remote_caller(),
                        reply,
                    });
                // No running session, or no dispatcher left to mint the
                // client: the process is past its last session, so the
                // socket is as good as gone.
                let Some(Some(accepted)) = answer else {
                    return;
                };
                // Counted before the answer is written and dropped once the
                // stream below ends, so a caller that reads its `Attached`
                // frame never sees this connection uncounted.
                let _counted = served.intake.attached();
                let attached = IpcResponse {
                    request_id,
                    result: IpcResult::Attached {
                        client_id: accepted.client_id,
                        session_id: accepted.session_id,
                        structure: accepted.structure,
                        resume_token: Some(accepted.resume_token),
                        pane_area: accepted.pane_area,
                    },
                };
                if connection.send(&attached).is_err() {
                    served.intake.hand_over(
                        inbox_tx,
                        RuntimeEvent::ClientDetached {
                            client_id: accepted.client_id,
                            detached_at: SystemTime::now(),
                            streamed: false,
                        },
                    );
                    return;
                }
                // The reply is written; from here the connection carries
                // the client's event stream and the client's own input.
                stream_events(
                    connection,
                    AttachedStream {
                        client_id: accepted.client_id,
                        events: accepted.events,
                        ending_notice: accepted.ending_notice,
                        graphics,
                    },
                    inbox_tx,
                    live_setting,
                    served,
                );
                return;
            }
            // A key press, a resize, a paste and a mouse round belong on an
            // attached client's connection, which the `Attach` arm above
            // hands to `stream_events`. On this control path they name no
            // client, so they close the connection.
            IpcRequestKind::KeyPress { .. }
            | IpcRequestKind::Resize { .. }
            | IpcRequestKind::Paste { .. }
            | IpcRequestKind::Mouse(_) => return,
            IpcRequestKind::Discovery => {
                let answer = ask_dispatcher(&served.intake, inbox_tx, |reply| {
                    RuntimeEvent::IpcDiscovery { reply }
                });
                // No running session: the process is past its last session, so
                // the socket is as good as gone.
                let Some(Some(overview)) = answer else {
                    return;
                };
                IpcResponse {
                    request_id,
                    result: IpcResult::Overview(overview),
                }
            }
            IpcRequestKind::Layout { tab } => {
                let answer = ask_dispatcher(&served.intake, inbox_tx, |reply| {
                    RuntimeEvent::IpcLayout { tab, reply }
                });
                // No running session: the process is past its last session, so
                // the socket is as good as gone.
                let Some(Some(layout)) = answer else {
                    return;
                };
                IpcResponse {
                    request_id,
                    result: IpcResult::Layout(layout),
                }
            }
            IpcRequestKind::RecentEvents => IpcResponse {
                request_id,
                result: IpcResult::RecentEvents(recent_events::recent()),
            },
            IpcRequestKind::Restart => {
                let answer = ask_dispatcher(&served.intake, inbox_tx, |reply| {
                    RuntimeEvent::IpcRestart { reply }
                });
                match answer {
                    Some(Ok(())) => IpcResponse {
                        request_id,
                        result: IpcResult::Restarting,
                    },
                    // The dispatcher named what is wrong; nothing was torn
                    // down, so the connection keeps serving.
                    Some(Err(message)) => IpcResponse {
                        request_id,
                        result: IpcResult::Error(IpcErrorPayload {
                            code: IpcErrorCode::MalformedRequest,
                            message,
                        }),
                    },
                    // No dispatcher left to swap anything: the process is
                    // tearing down, so the socket is as good as gone.
                    None => return,
                }
            }
            // No answer belongs to this one.
            IpcRequestKind::Leaving => return,
        };
        if connection.send(&response).is_err() {
            return;
        }
    }
}

/// Carry `client_id`'s event stream and input on its own connection until the
/// peer goes away, then detach the client.
///
/// The connection is split: a spawned thread drains the client's queue and
/// writes one frame per delivery that says something about the session's
/// structure, while this thread reads the client's own frames. A `KeyPress`, a
/// `Resize`, a `Paste`, a `SubmitCommand` and a `Mouse` round all cross to the
/// dispatcher over the inbox, and this half writes nothing back for any of
/// them: the first four are answered by the next painted frame, and a `Mouse`
/// round is answered on the writing half by exactly one
/// [`SessionEvent::MouseAnswer`] carrying that round's `request_id`. A request
/// of any other kind, end of stream, a transport fault, or a dispatcher that is
/// gone all end the reading loop.
///
/// A `SubmitCommand` on this connection has its source stamped by
/// [`client_source`], so it is attributed to `client_id` and to no other
/// client.
///
/// Either half ending detaches the client, which removes its record and drops
/// its subscription; the closed queue, or the terminal `Quit` or `Restarting`
/// frame, then ends the writing thread. Both notify, so a write that fails
/// while the reading half is still reading is cleaned up too — a detach for a
/// client already gone changes nothing.
///
/// A detach the server starts closes the client's queue: the writing thread
/// writes what is already queued, then [`SessionEvent::Detached`] as its last
/// frame, and ends. A session ending at the same moment drops what is queued,
/// and the goodbye is still what this client reads. The reading half keeps
/// reading until the client closes its end, and that close reads as end of
/// stream — a second detach for a client already gone. A `Leaving` request ends
/// the reading half the same way: the client says it sends nothing more, and
/// every key it sent has already been handed over.
///
/// `ending_notice` is what the session raises when it ends — see
/// [`EndingNotice`]. The writing thread reads it at the top of each turn and
/// writes the frame it names, [`SessionEvent::Quit`] or
/// [`SessionEvent::Restarting`], dropping anything still queued. A client whose
/// queue the server closed before that reads [`SessionEvent::Detached`]
/// instead: it left before the session did. The thread counts itself on the
/// notice for as long as it runs.
///
/// `live_setting` is the live read of the `allow-other-users` setting on a
/// connection from another local user, and `None` on one from the user who
/// started the session. It is read before each frame that client sends is
/// acted on, so turning the setting off detaches them at their next input.
///
/// This connection's place in the intake's attached count is held by the
/// caller: taken before the `Attached` frame is written, dropped once this
/// returns.
///
/// `served` is this connection's entry in the [`Intake`]: the intake holds this
/// connection's read direction through it. The client's record stays as it is,
/// so the image swap that cut the connection carries it across.
fn stream_events(
    connection: Connection,
    stream: AttachedStream,
    inbox_tx: &Sender<RuntimeEvent>,
    live_setting: Option<OtherUsersSetting>,
    served: &ServedConnection,
) {
    let AttachedStream {
        client_id,
        events,
        ending_notice,
        graphics,
    } = stream;
    let (mut reader, mut writer) = connection.split();
    let writer_inbox = inbox_tx.clone();
    let writer_intake = Arc::clone(&served.intake);
    ending_notice.writer_started();
    std::thread::spawn(move || {
        let mut image_cache = ConnectionImageCache::new();
        loop {
            // The session is ending. This client is told at once and whatever
            // is still queued for it goes unwritten. A queue the server already
            // closed detached this client before the session ended, so its own
            // goodbye is what it reads.
            if let Some(ending) = ending_notice.raised() {
                let frame = loop {
                    match events.try_recv() {
                        Ok(_) => {}
                        Err(mpsc::TryRecvError::Empty) => {
                            break match ending {
                                SessionEnding::Quit => SessionEvent::Quit,
                                SessionEnding::Restarting => SessionEvent::Restarting,
                            }
                        }
                        Err(mpsc::TryRecvError::Disconnected) => break SessionEvent::Detached,
                    }
                };
                let _ = writer.send(&frame);
                break;
            }
            let Ok(delivery) = events.recv() else {
                // The queue closed, which the server does when it detaches this
                // client. `recv` hands back everything queued before the close,
                // so the goodbye follows the events that preceded it.
                let _ = writer.send(&SessionEvent::Detached);
                break;
            };
            let write_failed = match &delivery {
                Delivery::Frame(snapshot) => {
                    send_painted_frame(&mut writer, &mut image_cache, snapshot, graphics, client_id)
                }
                _ => false,
            };
            if write_failed {
                break;
            }
            if matches!(delivery, Delivery::Frame(_)) {
                continue;
            }
            let event = wire_event(&delivery);
            if let Some(event) = event {
                let write_failed = match writer.send(&event) {
                    Ok(()) => false,
                    Err(IpcError::FrameTooLarge { len, max }) => {
                        tracing::warn!(%client_id, len, max, "frame over the cap was not sent");
                        false
                    }
                    Err(_) => true,
                };
                if write_failed {
                    break;
                }
                // `Quit` and `Restarting` are the stream's terminal frames; the
                // loop ends on either without waiting for the queue to close.
                if matches!(event, SessionEvent::Quit | SessionEvent::Restarting) {
                    break;
                }
            }
        }
        writer_intake.hand_over(
            &writer_inbox,
            RuntimeEvent::ClientDetached {
                client_id,
                detached_at: SystemTime::now(),
                streamed: true,
            },
        );
        ending_notice.writer_ended();
    });

    while let Ok(request) = reader.recv::<IncomingRequest>() {
        // The setting can change while this client is attached, so it is read
        // again for each frame that client sends.
        if live_setting.as_ref().is_some_and(|still_on| !still_on()) {
            break;
        }
        let kind = match request.kind {
            MaybeKnown::Known(kind) => kind,
            // A kind this build does not have comes from a newer koshi. This
            // connection carries the client's typing, so the one request is
            // dropped and the client keeps its stream.
            MaybeKnown::Unknown { name } => {
                tracing::debug!(%client_id, %name, "request kind this build does not have");
                continue;
            }
        };
        let event = match kind {
            IpcRequestKind::KeyPress { chord } => RuntimeEvent::ClientKeyPress { client_id, chord },
            IpcRequestKind::Resize {
                viewport,
                pane_area,
            } => RuntimeEvent::Resize {
                client_id,
                size: viewport,
                pane_area,
            },
            IpcRequestKind::Paste { text } => RuntimeEvent::HostPaste { client_id, text },
            IpcRequestKind::Mouse(actions) => RuntimeEvent::ClientMouse {
                client_id,
                request_id: request.request_id,
                actions,
            },
            IpcRequestKind::SubmitCommand(envelope) => {
                // The result is answered by the next painted frame. The reply
                // channel's receiving end drops here, and the dispatcher's send
                // into it fails.
                let (reply, _) = mpsc::channel();
                RuntimeEvent::Ipc {
                    envelope: client_source(*envelope, client_id),
                    reply,
                }
            }
            // The client sends nothing more on this connection, and everything
            // it did send is handed over above.
            IpcRequestKind::Leaving => break,
            IpcRequestKind::Hello { .. }
            | IpcRequestKind::Attach { .. }
            | IpcRequestKind::Discovery
            | IpcRequestKind::Layout { .. }
            | IpcRequestKind::RecentEvents
            | IpcRequestKind::Restart => break,
        };
        if !served.intake.hand_over(inbox_tx, event) {
            break;
        }
    }
    served.intake.hand_over(
        inbox_tx,
        RuntimeEvent::ClientDetached {
            client_id,
            detached_at: SystemTime::now(),
            streamed: true,
        },
    );
}

/// One image record retained while its placement remains in this connection's frame.
#[derive(Clone)]
struct CachedFrameImage {
    /// Identity used by painted frames and image transfer events.
    content_id: u64,
    /// The record uploaded for this identity. `None` remains a placeholder.
    record: Option<Arc<ImageRecord>>,
}

/// Image identities and records that belong to one attached connection.
struct ConnectionImageCache {
    /// Current records, keyed by pane and terminal-local placement identity.
    images: HashMap<(PaneId, u64), CachedFrameImage>,
    /// The next nonzero connection-local content identity.
    next_content_id: u64,
    /// Whether the next successful frame must invalidate the client's records.
    reset_required: bool,
}

impl ConnectionImageCache {
    /// Build an empty cache whose first image identity is 1.
    fn new() -> Self {
        Self {
            images: HashMap::new(),
            next_content_id: 1,
            reset_required: false,
        }
    }

    /// Assign stable content identities and list records this connection has not received.
    fn prepare(&mut self, snapshot: &RenderSnapshot) -> PreparedImageFrame {
        let placements = snapshot.panes.iter().flat_map(|pane| {
            pane.image_placements
                .iter()
                .map(move |placement| ((pane.id, placement.id()), placement))
        });
        let placements: Vec<((PaneId, u64), &ImagePlacementSnapshot)> = placements.collect();
        let changed_count = placements
            .iter()
            .filter(|(key, placement)| {
                self.images
                    .get(key)
                    .is_none_or(|cached| !same_record(cached.record.as_ref(), placement.record()))
            })
            .count();
        let available = if self.next_content_id == 0 {
            0
        } else {
            u64::MAX - self.next_content_id + 1
        };
        let reset = self.reset_required
            || u64::try_from(changed_count).map_or(true, |count| count > available);
        if reset {
            self.images.clear();
            self.next_content_id = 1;
            self.reset_required = false;
        }

        let mut uploads = Vec::new();
        let mut retained = HashSet::with_capacity(placements.len());
        let mut content_by_record: HashMap<*const ImageRecord, u64> = self
            .images
            .values()
            .filter_map(|cached| {
                cached
                    .record
                    .as_ref()
                    .map(|record| (Arc::as_ptr(record), cached.content_id))
            })
            .collect();
        for (key, placement) in placements {
            retained.insert(key);
            let record = placement.record_arc();
            let unchanged = self
                .images
                .get(&key)
                .is_some_and(|cached| same_record(cached.record.as_ref(), record.as_deref()));
            if unchanged {
                continue;
            }
            let content_id = record
                .as_ref()
                .and_then(|record| content_by_record.get(&Arc::as_ptr(record)).copied())
                .unwrap_or_else(|| {
                    let content_id = self.next_content_id;
                    self.next_content_id = self.next_content_id.checked_add(1).unwrap_or(0);
                    if let Some(record) = record.as_ref() {
                        content_by_record.insert(Arc::as_ptr(record), content_id);
                        uploads.push((content_id, Arc::clone(record)));
                    }
                    content_id
                });
            self.images
                .insert(key, CachedFrameImage { content_id, record });
        }
        self.images.retain(|key, _| retained.contains(key));

        let frame = wire_frame_with_content_ids(snapshot, |pane_id, placement| {
            self.images
                .get(&(pane_id, placement.id()))
                .map_or(placement.content_id(), |cached| cached.content_id)
        });
        PreparedImageFrame {
            reset,
            frame,
            uploads,
        }
    }

    /// Forget all connection-local image identities after a recoverable write failure.
    fn clear(&mut self) {
        self.images.clear();
        self.next_content_id = 1;
        self.reset_required = true;
    }
}

/// A painted frame plus the image records its connection still needs.
struct PreparedImageFrame {
    /// Whether the client must discard every record before this frame.
    reset: bool,
    /// Placement geometry and connection-local content identities.
    frame: PaintedFrame,
    /// New content identities and the records uploaded under them.
    uploads: Vec<(u64, Arc<ImageRecord>)>,
}

/// Report whether two cached record slots hold the same retained image record.
fn same_record(cached: Option<&Arc<ImageRecord>>, incoming: Option<&ImageRecord>) -> bool {
    match (cached, incoming) {
        (Some(cached), Some(incoming)) => std::ptr::eq(cached.as_ref(), incoming),
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

/// Send one painted frame and each new image record after it.
fn send_painted_frame(
    writer: &mut FrameWriter,
    cache: &mut ConnectionImageCache,
    snapshot: &RenderSnapshot,
    graphics: GraphicsCapabilities,
    client_id: ClientId,
) -> bool {
    if !graphics.kitty {
        let event = SessionEvent::Painted {
            frame: Box::new(wire_frame(snapshot)),
        };
        return match writer.send(&event) {
            Ok(()) => false,
            Err(error) => report_image_send_error(cache, client_id, error, "the painted frame"),
        };
    }
    let prepared = cache.prepare(snapshot);
    if prepared.reset {
        if let Err(error) = writer.send(&SessionEvent::ImageCacheReset) {
            return report_image_send_error(cache, client_id, error, "the image cache reset");
        }
    }
    let repeated_frame =
        (prepared.uploads.len() > MAX_FRAME_IMAGE_TRANSFERS).then(|| prepared.frame.clone());
    let event = SessionEvent::Painted {
        frame: Box::new(prepared.frame),
    };
    if let Err(error) = writer.send(&event) {
        return report_image_send_error(cache, client_id, error, "the painted frame");
    }
    for (index, (content_id, record)) in prepared.uploads.into_iter().enumerate() {
        if index != 0 && index % MAX_FRAME_IMAGE_TRANSFERS == 0 {
            let event = SessionEvent::Painted {
                frame: Box::new(
                    repeated_frame
                        .as_ref()
                        .expect("a repeated image batch retained its painted frame")
                        .clone(),
                ),
            };
            if let Err(error) = writer.send(&event) {
                return report_image_send_error(cache, client_id, error, "the painted frame");
            }
        }
        let event = SessionEvent::ImageContentStart {
            image: wire_image_transfer(content_id, &record),
        };
        if let Err(error) = writer.send(&event) {
            return report_image_send_error(cache, client_id, error, "an image transfer start");
        }
        for (offset, last, bytes) in wire_image_chunk_sources(&record) {
            let event = SessionEvent::ImageContentChunk {
                chunk: FrameImageChunk {
                    transfer_id: content_id,
                    offset,
                    last,
                    bytes: bytes.to_vec(),
                },
            };
            if let Err(error) = writer.send(&event) {
                return report_image_send_error(cache, client_id, error, "an image transfer chunk");
            }
        }
    }
    false
}

/// Report one image-stream write failure and say whether the connection stays usable.
fn report_image_send_error(
    cache: &mut ConnectionImageCache,
    client_id: ClientId,
    error: IpcError,
    part: &str,
) -> bool {
    match error {
        IpcError::FrameTooLarge { len, max } => {
            tracing::warn!(%client_id, %part, len, max, "image transfer part exceeded the frame cap");
            cache.clear();
            false
        }
        error => {
            tracing::warn!(%client_id, %part, %error, "image transfer write failed");
            true
        }
    }
}

/// Rebuild `envelope` with the source a control connection carries, over
/// whatever source its sender wrote.
///
/// A control connection carries a `koshi` CLI invocation. Its two sources,
/// [`CommandSource::InSessionCli`] and [`CommandSource::ExternalCli`], are kept
/// as they stand. Every other source becomes
/// `ExternalCli { session_id: None, target_client: None }`, which names no
/// session and no client.
///
/// The envelope's `client_id` is re-derived from the stamped source; the two
/// always agree.
///
/// A sender that writes `CommandSource::Internal` and
/// `Command::ToggleMouseSelect` reaches the dispatcher as
/// `ExternalCli { session_id: None, target_client: None }` carrying
/// `Command::ToggleMouseSelect`. The dispatcher's CLI-admission check refuses
/// it: the CLI has no mouse-select verb.
fn cli_source(envelope: CommandEnvelope) -> CommandEnvelope {
    let source = match envelope.source {
        source @ (CommandSource::InSessionCli { .. } | CommandSource::ExternalCli { .. }) => source,
        CommandSource::KeyBinding { .. }
        | CommandSource::Mouse { .. }
        | CommandSource::Plugin { .. }
        | CommandSource::Internal => CommandSource::external_cli(None, None),
    };
    CommandEnvelope::new(envelope.id, source, envelope.issued_at, envelope.command)
}

/// Rebuild `envelope` with [`CommandSource::KeyBinding`] naming `client_id`,
/// over whatever source its sender wrote.
///
/// `client_id` is the client this connection attached as. A command this
/// connection sends is attributed to that client and to no other.
///
/// The envelope's `client_id` is re-derived from the stamped source; the two
/// always agree.
fn client_source(envelope: CommandEnvelope, client_id: ClientId) -> CommandEnvelope {
    CommandEnvelope::new(
        envelope.id,
        CommandSource::key_binding(client_id),
        envelope.issued_at,
        envelope.command,
    )
}

/// Hand one request to the dispatcher thread and wait for its answer: build
/// the inbox event around a fresh reply channel, hand it over the intake, and
/// block on the reply. `None` means no answer is coming — the intake is closed,
/// or the dispatcher is gone — so the caller closes its connection without one.
fn ask_dispatcher<T>(
    intake: &Intake,
    inbox_tx: &Sender<RuntimeEvent>,
    build_event: impl FnOnce(mpsc::Sender<T>) -> RuntimeEvent,
) -> Option<T> {
    let (reply_tx, reply_rx) = mpsc::channel();
    if !intake.hand_over(inbox_tx, build_event(reply_tx)) {
        return None;
    }
    reply_rx.recv().ok()
}

#[cfg(test)]
mod tests;
