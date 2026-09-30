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
//! the machine-wide shared directory and widens it: the other local users of
//! this machine can reach it. The endpoint file keeps its private place and
//! its `0600` mode either way. Every accepted connection is gated by the user
//! the OS reports for it: the user who started the session is always served,
//! and another local user is served only while `allow-other-users` is on. The
//! setting is read again for each of that user's requests. Turning it off
//! closes their connections.
//!
//! The decisions every koshi server makes the same way — a request kind this
//! build does not have, a malformed-but-aligned frame, an oversize frame, and
//! the Hello — belong to [`plane::read_next_request`], which this loop reads its
//! requests through. None of them reaches the session, any pane, or any other
//! connection.
//!
//! An attached client's connection is read on a loop of its own, which writes
//! no frame back. That loop reads the `allow-other-users` setting after each
//! read on the connection, whether or not the frame decoded, then drops a
//! request kind this build does not have and a malformed-but-aligned frame. It
//! ends on an oversize frame, a disconnect and a transport fault.
//!
//! A `Leaving` request ends the connection it arrives on: the thread serving it
//! stops reading and the connection closes. Requests arrive in the order the
//! peer queued them, so every request that peer sent is already with the
//! dispatcher by then. [`IpcServer::count_attached_connections`] counts the attached
//! clients still being read.
//!
//! [`IpcServer::close_intake`] ends the connections that are left: no event a
//! peer sends from here is handed to the dispatcher, and every connection being
//! served has its read direction closed. Its thread ends without reading
//! another request. The dispatcher calls it before it stops draining the inbox.
//!
//! [`IpcServer::shutdown`] stops accepting, joins the accept loop, and
//! removes the endpoint file, the socket and any shared marker.

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
    compute_socket_address, remove_advertisement_marker, remove_socket_file,
    resolve_advertisement_marker_path, write_advertisement_marker, EndpointFile,
};
use koshi_ipc::error::IpcError;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::{
    FrameImageChunk, FrameImagePlacement, PaintedFrame, MAX_FRAME_IMAGE_TRANSFER_COUNT,
};
use koshi_ipc::handshake::{Handshake, Peer};
use koshi_ipc::plane::{self, RequestDisposition};
use koshi_ipc::protocol::{
    ConnectionToken, GraphicsCapabilities, IncomingRequest, IpcErrorCode, IpcErrorPayload,
    IpcRequestKind, IpcResponse, IpcResult, SessionPlane,
};
use koshi_ipc::transport::{self, Connection, FrameWriter, Listener, ReadCloser};
use koshi_ipc::validate::{
    reclaim_stale_socket, validate_shared_socket_address, validate_socket_address,
};
use koshi_ipc::wire::MaybeKnown;
use koshi_observability::logging::recent_events;
use koshi_renderer::snapshot::{
    Delivery, ImagePlacementSnapshot, PlacementSnapshot, RenderSnapshot,
};
use koshi_terminal::graphics::ImageRecord;

use crate::runtime::bus::build_wire_event;
use crate::runtime::event::{EndingNotice, RuntimeEvent, SessionEnding};
use crate::runtime::frame::{
    build_wire_frame, build_wire_frame_with_content_ids, build_wire_image_chunk_sources,
    build_wire_image_transfer, build_wire_placement_snapshot,
};

/// How long the accept loop sleeps after a failed accept before it accepts
/// again. A persistent accept error — say, the process is out of file
/// descriptors — retries once per interval.
const ACCEPT_RETRY_DELAY_DURATION: Duration = Duration::from_millis(100);

/// The version of the binary this session server is, reported in its Hello
/// answer.
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Reads the `allow-other-users` setting from the configuration again and
/// reports whether it is on. Called for each connection from another local
/// user and for each request that user sends.
pub type OtherUsersSetting = Arc<dyn Fn() -> bool + Send + Sync>;

/// What [`IpcServer::start`] needs to serve the other local users of this
/// machine.
///
/// Only the control socket moves: it binds in this user's directory under the
/// machine-wide shared directory, carrying mode `0666` on Unix and a security
/// descriptor granting Authenticated Users read and write on Windows. The
/// endpoint file stays in the private runtime directory at mode `0600`: the
/// token it carries stays readable only by the user who started the session.
pub struct OtherUsers {
    /// The machine-wide directory koshi shares between local users, from
    /// `koshi_paths::resolve_shared_sessions_directory`. This user's directory inside it
    /// holds the control socket.
    pub shared_directory: PathBuf,
    /// The live read of the `allow-other-users` setting.
    pub is_enabled: OtherUsersSetting,
}

/// What the control socket takes in: every connection being served, how many of
/// them carry an attached client, and whether what a peer sends still reaches
/// the session.
///
/// A serving thread reads `is_closed` and hands its event to the dispatcher under
/// one shared borrow of the intake, and [`close_intake`](Intake::close_intake) sets
/// `is_closed` under the exclusive borrow. A hand-off either finishes before the
/// close, or reads the closed intake and never happens. Once
/// [`close_intake`](Intake::close_intake) returns, no serving thread sends another event.
#[derive(Debug, Default)]
struct Intake {
    intake_state: RwLock<IntakeState>,
}

/// What an [`Intake`] holds.
#[derive(Debug, Default)]
struct IntakeState {
    /// `true` once [`Intake::close_intake`] ran. No event reaches the dispatcher from
    /// here.
    is_closed: bool,
    /// How many connections carry an attached client and are still being read.
    attached_connection_count: usize,
    /// The read direction of every connection being served, under the number
    /// the intake filed it as. The [`ServedConnection`] its thread holds
    /// removes the entry when that thread ends.
    reader_by_connection_number: HashMap<u64, ReadCloser>,
    /// The number the next accepted connection is filed under.
    next_connection_number: u64,
}

impl Intake {
    /// File `connection`'s read direction and hand back the entry its serving
    /// thread holds. `None` means the connection is not served: intake is
    /// closed, or its read direction could not be taken.
    fn accept_connection(self: &Arc<Self>, connection: &Connection) -> Option<ServedConnection> {
        let reader = connection.create_read_closer().ok()?;
        let mut intake_state = self.intake_state.write().expect("intake");
        if intake_state.is_closed {
            return None;
        }
        let connection_number = intake_state.next_connection_number;
        intake_state.next_connection_number += 1;
        intake_state
            .reader_by_connection_number
            .insert(connection_number, reader);
        Some(ServedConnection {
            intake: Arc::clone(self),
            connection_number,
        })
    }

    /// Hand `event` to the dispatcher over the runtime inbox. `false` means it
    /// was not handed over — intake is closed, or the dispatcher is gone — and
    /// the caller's connection ends.
    fn hand_over_event(
        &self,
        inbox_sender: &Sender<RuntimeEvent>,
        runtime_event: RuntimeEvent,
    ) -> bool {
        let intake_state = self.intake_state.read().expect("intake");
        !intake_state.is_closed && inbox_sender.send(runtime_event).is_ok()
    }

    /// Count one attached client's connection as being read, and hand back the
    /// entry the thread reading it holds.
    fn record_attached_connection(self: &Arc<Self>) -> AttachedConnection {
        self.intake_state
            .write()
            .expect("intake")
            .attached_connection_count += 1;
        AttachedConnection {
            intake: Arc::clone(self),
        }
    }

    /// How many connections carry an attached client and are still being read.
    fn count_attached_connections(&self) -> usize {
        self.intake_state
            .read()
            .expect("intake")
            .attached_connection_count
    }

    /// Take connections again after a [`close_intake`](Intake::close_intake): a connection
    /// accepted from here on is served, and a serving thread hands its events
    /// over again.
    ///
    /// The connections closed by that call stay closed.
    fn reopen_intake(&self) {
        self.intake_state.write().expect("intake").is_closed = false;
    }

    /// Stop events reaching the dispatcher and close the read direction of
    /// every connection being served. Each serving thread ends without reading
    /// another request.
    fn close_intake(&self) {
        let mut intake_state = self.intake_state.write().expect("intake");
        intake_state.is_closed = true;
        for reader in intake_state.reader_by_connection_number.values() {
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
        self.intake
            .intake_state
            .write()
            .expect("intake")
            .attached_connection_count -= 1;
    }
}

/// One connection's entry in the [`Intake`], held by the thread serving that
/// connection. The entry is removed when that thread ends. The intake holds the
/// read direction of the connections being served and no others.
struct ServedConnection {
    /// The intake this entry belongs to, and the gate the serving thread hands
    /// its events over through.
    intake: Arc<Intake>,
    /// The number this connection is filed under.
    connection_number: u64,
}

impl Drop for ServedConnection {
    fn drop(&mut self) {
        self.intake
            .intake_state
            .write()
            .expect("intake")
            .reader_by_connection_number
            .remove(&self.connection_number);
    }
}

/// The accepted client state consumed by one attached event stream.
struct AttachedStream {
    /// Client whose input and output use this stream.
    client_id: ClientId,
    /// Deliveries waiting to be written to the client.
    deliveries: Receiver<Delivery>,
    /// Shared session-ending notice read by the writer.
    ending_notice: Arc<EndingNotice>,
    /// Graphics capabilities reported by this connection.
    graphics_capabilities: GraphicsCapabilities,
}

/// The serving side of one session's control socket: the bound listener's
/// accept loop, the address it serves, and the endpoint file advertising it.
///
/// Held by the server for the session's lifetime; [`shutdown`](Self::shutdown)
/// stops the loop and withdraws the files it wrote.
#[derive(Debug)]
pub struct IpcServer {
    /// The control-socket address the accept loop is serving.
    socket_address: String,
    /// The endpoint file advertising `socket_address` and the connection token.
    endpoint_path: PathBuf,
    /// The empty marker naming this session among those other local users may
    /// reach, written on Windows where a pipe has no filesystem entry. `None`
    /// on Unix, and `None` for a session only its own user may reach.
    shared_socket_marker_path: Option<PathBuf>,
    /// Set by [`shutdown`](Self::shutdown); the accept loop exits when it
    /// observes the flag.
    is_shutting_down: Arc<AtomicBool>,
    /// The secret a connection presents at Hello, shared with the accept loop,
    /// which reads it for each connection it accepts.
    connection_token: Arc<RwLock<ConnectionToken>>,
    /// What the socket takes in, shared with every serving thread.
    connection_intake: Arc<Intake>,
    /// The accept loop, joined at shutdown. `None` once
    /// [`stop_ipc_server`](Self::stop_ipc_server) has taken it out to join it.
    accept_thread: Option<JoinHandle<()>>,
}

impl IpcServer {
    /// Bind the control socket of `session_id`, write the endpoint file
    /// advertising it, and start serving.
    ///
    /// The steps run in trust order: the runtime directory is created
    /// private (`0700`), the address is checked against the directory it sits
    /// in, any stale leftover socket is reclaimed, the listener binds, and
    /// only then is the endpoint file written. The advertisement never exists
    /// without a listener behind it. A failed endpoint write unwinds the bind
    /// and leaves nothing behind.
    ///
    /// `other_users` `None` binds the socket inside `runtime_directory`, where only
    /// the user who started the session can reach it. `Some` binds it in this
    /// user's directory under the machine-wide shared directory instead and
    /// opens it to the other local users: mode `0666` on Unix, a pipe carrying
    /// the Authenticated Users access of [`Listener::bind_shared`] on Windows,
    /// where the marker naming the pipe is written as well. The endpoint file
    /// is written to the same private path at the same `0600` mode either way;
    /// only the address it carries differs.
    pub fn start(
        runtime_directory: &Path,
        session_id: SessionId,
        inbox_sender: Sender<RuntimeEvent>,
        other_users: Option<OtherUsers>,
    ) -> Result<IpcServer, IpcError> {
        koshi_paths::ensure_private_directory(runtime_directory).map_err(|directory_error| {
            IpcError::Transport {
                error_detail: format!(
                    "could not create the runtime directory {}: {directory_error}",
                    runtime_directory.display()
                ),
            }
        })?;
        let (socket_address, shared_socket_marker_path) = match &other_users {
            None => {
                let socket_address = compute_socket_address(runtime_directory, session_id);
                validate_socket_address(&socket_address, runtime_directory)?;
                (socket_address, None)
            }
            Some(other_users) => {
                let shared_user_directory =
                    ensure_shared_directories(&other_users.shared_directory)?;
                let socket_address = compute_socket_address(&shared_user_directory, session_id);
                validate_shared_socket_address(&socket_address, &shared_user_directory)?;
                // On Windows, a marker file names the session listening on the
                // pipe. A pipe has no filesystem entry.
                let shared_socket_marker_path = cfg!(windows)
                    .then(|| resolve_advertisement_marker_path(&shared_user_directory, session_id));
                (socket_address, shared_socket_marker_path)
            }
        };
        reclaim_stale_socket(&socket_address)?;
        let listener = if other_users.is_some() {
            Listener::bind_shared(&socket_address)?
        } else {
            Listener::bind(&socket_address)?
        };
        #[cfg(unix)]
        if other_users.is_some() {
            if let Err(socket_error) = widen_socket(&socket_address) {
                drop(listener);
                remove_socket_file(&socket_address);
                return Err(socket_error);
            }
        }

        let connection_token = ConnectionToken::generate();
        let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
        let endpoint_file = EndpointFile {
            socket_address: socket_address.clone(),
            connection_token: connection_token.clone(),
            process_id: std::process::id(),
        };
        let advertisement_result = endpoint_file.write_to_path(&endpoint_path).and_then(|()| {
            match &shared_socket_marker_path {
                None => Ok(()),
                Some(shared_socket_marker_path) => {
                    write_advertisement_marker(shared_socket_marker_path)
                }
            }
        });
        if let Err(advertisement_error) = advertisement_result {
            // Dropping the listener releases the address and unlinks the socket
            // file on Unix. The endpoint write is atomic: a file left at
            // `endpoint_path` is an older run's, naming the socket removed here.
            let _ = std::fs::remove_file(&endpoint_path);
            drop(listener);
            remove_socket_file(&socket_address);
            return Err(advertisement_error);
        }

        let allow_other_users_setting = other_users.map(|other_users| other_users.is_enabled);
        let is_shutting_down = Arc::new(AtomicBool::new(false));
        let accept_shutdown_flag = Arc::clone(&is_shutting_down);
        let connection_intake = Arc::new(Intake::default());
        let accept_intake = Arc::clone(&connection_intake);
        let connection_token = Arc::new(RwLock::new(connection_token));
        let accept_connection_token = Arc::clone(&connection_token);
        let accept_thread = std::thread::spawn(move || {
            run_accept_loop(
                &listener,
                &accept_connection_token,
                &inbox_sender,
                &accept_shutdown_flag,
                allow_other_users_setting.as_ref(),
                &accept_intake,
            );
        });

        Ok(IpcServer {
            socket_address,
            endpoint_path,
            shared_socket_marker_path,
            is_shutting_down,
            connection_token,
            connection_intake,
            accept_thread: Some(accept_thread),
        })
    }

    /// End every connection still being served: no event a peer sends from here
    /// is handed to the dispatcher, and each connection has its read direction
    /// closed. The thread serving it ends without reading another request.
    ///
    /// A request a peer sent before this either reached the dispatcher already
    /// or never does. The dispatcher's next pass over the runtime inbox is the
    /// last that can find anything a peer sent.
    ///
    /// A peer still holding its connection loses what it wrote and the session
    /// had not read, and sees its connection close.
    ///
    /// A connection accepted after this is closed without being served.
    /// Closing an already-closed intake changes nothing.
    pub fn close_intake(&self) {
        self.connection_intake.close_intake();
    }

    /// Mint a fresh connection token, accept it from the next connection on,
    /// and rewrite the endpoint file with it.
    ///
    /// The address does not change. A connection already open keeps serving:
    /// the token is checked at Hello only. An intake closed by
    /// [`close_intake`](Self::close_intake) takes connections again.
    ///
    /// # Errors
    /// Returns the failure of writing the endpoint file. The fresh token is
    /// the one this server accepts either way: the endpoint file then
    /// advertises a token no connection is served under.
    pub fn rotate_token(&self) -> Result<(), IpcError> {
        let fresh_connection_token = ConnectionToken::generate();
        // The fresh token is stored before the intake reopens and before the
        // endpoint file advertises it.
        *self.connection_token.write().expect("connection token") = fresh_connection_token.clone();
        self.connection_intake.reopen_intake();
        let endpoint_file = EndpointFile {
            socket_address: self.socket_address.clone(),
            connection_token: fresh_connection_token,
            process_id: std::process::id(),
        };
        endpoint_file.write_to_path(&self.endpoint_path)
    }

    /// How many attached clients' connections are still being read.
    ///
    /// A client that read the session's `Restarting` frame sends `Leaving` and
    /// writes nothing after it, and its connection ends once the session has
    /// read everything it sent. After that frame, `0` means every attached
    /// client's input is in the runtime inbox.
    #[must_use]
    pub fn count_attached_connections(&self) -> usize {
        self.connection_intake.count_attached_connections()
    }

    /// The control-socket address this server is serving.
    #[must_use]
    pub fn get_socket_address(&self) -> &str {
        &self.socket_address
    }

    /// Stop serving: no further connection is accepted, the accept loop is
    /// joined, and the endpoint file, the socket and any shared marker are
    /// removed. Connections already being served run out on their own threads;
    /// with the dispatcher draining, their in-flight requests end in a closed
    /// connection rather than a mutation.
    ///
    /// Dropping an `IpcServer` runs the same teardown, including when a panic
    /// unwinds the server.
    pub fn shutdown(self) {
        drop(self);
    }

    /// The teardown itself, safe to run at most once per field: the join is
    /// guarded by taking `accept_thread`, and removing an already-removed
    /// file is a no-op.
    fn stop_ipc_server(&mut self) {
        self.is_shutting_down.store(true, Ordering::SeqCst);
        if let Some(accept_thread_handle) = self.accept_thread.take() {
            // The accept loop sits blocked in `accept`. A bare connect wakes it
            // and it reads the flag. That connection stays open until the join
            // returns. A failed connect — say, the process is out of file
            // descriptors — leaves the loop blocked and skips the join; that
            // thread ends with the process, and the files below are removed
            // either way.
            if let Ok(wake_connection) = Connection::connect(&self.socket_address) {
                let _ = accept_thread_handle.join();
                drop(wake_connection);
            }
        }
        let _ = std::fs::remove_file(&self.endpoint_path);
        if let Some(shared_socket_marker_path) = &self.shared_socket_marker_path {
            remove_advertisement_marker(shared_socket_marker_path);
        }
        remove_socket_file(&self.socket_address);
    }
}

/// Create the machine-wide shared directory and this user's directory inside
/// it, and hand back this user's directory: where a session other local users
/// may reach binds its control socket.
fn ensure_shared_directories(shared_directory: &Path) -> Result<PathBuf, IpcError> {
    koshi_paths::ensure_shared_base(shared_directory).map_err(|directory_error| {
        IpcError::Transport {
            error_detail: format!(
                "could not create the shared session directory {}: {directory_error}",
                shared_directory.display()
            ),
        }
    })?;
    koshi_paths::ensure_shared_user_directory(shared_directory).map_err(|directory_error| {
        IpcError::Transport {
            error_detail: format!(
                "could not create this user's directory under {}: {directory_error}",
                shared_directory.display()
            ),
        }
    })
}

/// Set the socket file at `socket_address` to mode `0666`: every local user of
/// this machine may connect to it. Unix only: on Windows the address is a pipe
/// name with no filesystem entry.
#[cfg(unix)]
fn widen_socket(socket_address: &str) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(socket_address, std::fs::Permissions::from_mode(0o666)).map_err(
        |permission_error| IpcError::Transport {
            error_detail: format!(
                "could not widen the control socket {socket_address}: {permission_error}"
            ),
        },
    )
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop_ipc_server();
    }
}

/// Accept connections until the shutdown flag is set, giving each its own
/// serving thread. A failed accept pauses for [`ACCEPT_RETRY_DELAY_DURATION`]
/// and retries.
///
/// `allow_other_users_setting` is the live read of the `allow-other-users`
/// setting, and is `None` for a session only its own user may reach. Each
/// connection is gated as [`Peer::Local`] with the user the OS reports for it
/// and the setting read at accept. The handshake refuses another local user
/// arriving while the setting is off with `OtherUsersOff`. A connection whose
/// user cannot be read is closed without being served.
///
/// `connection_intake` files each connection's read direction before its thread
/// starts, and is what that thread hands its events over through. A connection
/// the intake does not take — it is closed, or the read direction could not be
/// taken — is closed without being served.
fn run_accept_loop(
    listener: &Listener,
    connection_token: &RwLock<ConnectionToken>,
    inbox_sender: &Sender<RuntimeEvent>,
    is_shutting_down: &AtomicBool,
    allow_other_users_setting: Option<&OtherUsersSetting>,
    connection_intake: &Arc<Intake>,
) {
    transport::accept_until_shutdown(
        listener,
        is_shutting_down,
        ACCEPT_RETRY_DELAY_DURATION,
        |connection| {
            // The OS reports which user opened the connection.
            let Ok(is_same_user) = connection.is_peer_same_user() else {
                return;
            };
            let Some(served_connection) = connection_intake.accept_connection(&connection) else {
                return;
            };
            let peer = Peer::Local {
                is_same_user,
                is_other_user_access_allowed: allow_other_users_setting
                    .is_some_and(|is_enabled| is_enabled()),
            };
            // Another local user's connection carries the setting. Each of
            // their requests reads it again.
            let live_setting = if is_same_user {
                None
            } else {
                allow_other_users_setting.cloned()
            };
            // The token is read for each connection: the next Hello is
            // checked against the token rotated last.
            let connection_token = connection_token.read().expect("connection token").clone();
            let inbox_sender = inbox_sender.clone();
            std::thread::spawn(move || {
                serve_connection(
                    connection,
                    connection_token,
                    &inbox_sender,
                    peer,
                    live_setting,
                    &served_connection,
                );
            });
        },
    );
}

/// Serve one connection until its peer hangs up or a fault closes it.
///
/// [`plane::read_next_request`] makes every decision that is the same on every
/// koshi protocol — the framing faults, a request kind this build does not
/// have, and the Hello — and reads `live_setting` before any of its answers go
/// out. What is left is this session's own vocabulary: `SubmitCommand`,
/// `Attach`, `Discovery`, `Layout` and `Restart` cross to the dispatcher over
/// the inbox and answer with its reply. `RecentEvents` is answered on this
/// thread, from the process-wide log ring, and reaches no dispatcher.
///
/// A `SubmitCommand` on this connection has its source stamped by
/// [`stamp_cli_command_source`] before it crosses to the dispatcher: this connection carries
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
/// answer goes out. Turning the setting off closes the connection with nothing
/// written.
///
/// A `Leaving` request ends the connection with no answer.
///
/// `served_connection` is this connection's entry in the [`Intake`]: the intake
/// holds this connection's read direction through it, and the entry is removed
/// when this function returns. An intake that closes ends the connection,
/// whatever the request was.
fn serve_connection(
    mut connection: Connection,
    connection_token: ConnectionToken,
    inbox_sender: &Sender<RuntimeEvent>,
    peer: Peer,
    live_setting: Option<OtherUsersSetting>,
    served_connection: &ServedConnection,
) {
    let mut handshake = Handshake::from_expected_token_and_peer(connection_token, peer);
    // The setting is read again for each request. `None` is a connection from
    // the user who started the session, whose admission cannot be withdrawn.
    let admission_setting = live_setting.clone();
    let is_admitted = move || match &admission_setting {
        None => true,
        Some(is_enabled) => is_enabled(),
    };
    loop {
        let (request_id, request_kind) = match plane::read_next_request::<SessionPlane>(
            &mut connection,
            &mut handshake,
            BUILD_VERSION,
            &is_admitted,
        ) {
            RequestDisposition::Answered => continue,
            RequestDisposition::Stop => return,
            RequestDisposition::Dispatch {
                request_id,
                request_kind,
            } => (Some(request_id), request_kind),
        };

        let outgoing_response = match request_kind {
            IpcRequestKind::Hello { .. } => {
                unreachable!("Hello is answered by the connection thread before dispatch")
            }
            IpcRequestKind::SubmitCommand(command_envelope) => {
                let stamped_command_envelope =
                    Box::new(stamp_cli_command_source(*command_envelope));
                let dispatch_response = request_dispatcher_response(
                    &served_connection.intake,
                    inbox_sender,
                    |response_sender| RuntimeEvent::Ipc {
                        command_envelope: stamped_command_envelope,
                        response_sender,
                    },
                );
                let Some(command_result) = dispatch_response else {
                    return;
                };
                IpcResponse {
                    request_id,
                    answer_result: IpcResult::CommandResult(command_result),
                }
            }
            IpcRequestKind::Attach {
                viewport_size,
                resume_client_id,
                resume_token,
                pane_area,
                graphics_capabilities,
                cell_size,
            } => {
                let dispatch_response = request_dispatcher_response(
                    &served_connection.intake,
                    inbox_sender,
                    |response_sender| RuntimeEvent::IpcAttach {
                        resume_client_id,
                        resume_token,
                        viewport_size,
                        pane_area,
                        cell_size,
                        attached_at: SystemTime::now(),
                        is_remote: handshake.is_remote_caller(),
                        response_sender,
                    },
                );
                // No running session, or no dispatcher left to mint the
                // client: the connection closes with no answer.
                let Some(Some(accepted_client)) = dispatch_response else {
                    return;
                };
                // Counted before the answer is written and dropped once the
                // stream below ends. A caller that reads its `Attached` frame
                // finds this connection counted.
                let _attached_connection_guard =
                    served_connection.intake.record_attached_connection();
                let attached_response = IpcResponse {
                    request_id,
                    answer_result: IpcResult::Attached {
                        client_id: accepted_client.client_id,
                        session_id: accepted_client.session_id,
                        session_structure: accepted_client.session_structure,
                        resume_token: Some(accepted_client.resume_token),
                        pane_area: accepted_client.pane_area,
                    },
                };
                if connection.send(&attached_response).is_err() {
                    served_connection.intake.hand_over_event(
                        inbox_sender,
                        RuntimeEvent::ClientDetached {
                            client_id: accepted_client.client_id,
                            detached_at: SystemTime::now(),
                            is_streamed: false,
                        },
                    );
                    return;
                }
                // The reply is written; from here the connection carries
                // the client's event stream and the client's own input.
                stream_events(
                    connection,
                    AttachedStream {
                        client_id: accepted_client.client_id,
                        deliveries: accepted_client.deliveries,
                        ending_notice: accepted_client.ending_notice,
                        graphics_capabilities,
                    },
                    inbox_sender,
                    live_setting,
                    served_connection,
                );
                return;
            }
            // A keyboard event, a resize, a paste and a mouse round belong on
            // an attached client's connection, which the `Attach` arm above
            // hands to `stream_events`. On this control path they name no
            // client and close the connection.
            IpcRequestKind::Keyboard { .. }
            | IpcRequestKind::Resize { .. }
            | IpcRequestKind::CellSize { .. }
            | IpcRequestKind::Paste { .. }
            | IpcRequestKind::Mouse(_)
            | IpcRequestKind::ReadPanePlacement { .. } => return,
            IpcRequestKind::Discovery => {
                let dispatch_response = request_dispatcher_response(
                    &served_connection.intake,
                    inbox_sender,
                    |response_sender| RuntimeEvent::IpcDiscovery { response_sender },
                );
                // No running session: the connection closes with no answer.
                let Some(Some(session_overview)) = dispatch_response else {
                    return;
                };
                IpcResponse {
                    request_id,
                    answer_result: IpcResult::Overview(session_overview),
                }
            }
            IpcRequestKind::Layout { tab_id } => {
                let dispatch_response = request_dispatcher_response(
                    &served_connection.intake,
                    inbox_sender,
                    |response_sender| RuntimeEvent::IpcLayout {
                        tab_id,
                        response_sender,
                    },
                );
                // No running session: the connection closes with no answer.
                let Some(Some(session_layout)) = dispatch_response else {
                    return;
                };
                IpcResponse {
                    request_id,
                    answer_result: IpcResult::Layout(session_layout),
                }
            }
            IpcRequestKind::RecentEvents => IpcResponse {
                request_id,
                answer_result: IpcResult::RecentEvents(recent_events::list_recent_events()),
            },
            IpcRequestKind::Restart => {
                let dispatch_response = request_dispatcher_response(
                    &served_connection.intake,
                    inbox_sender,
                    |response_sender| RuntimeEvent::IpcRestart { response_sender },
                );
                match dispatch_response {
                    Some(Ok(())) => IpcResponse {
                        request_id,
                        answer_result: IpcResult::Restarting,
                    },
                    // The dispatcher named what is wrong. Nothing was torn
                    // down, and the connection keeps serving.
                    Some(Err(restart_error_message)) => IpcResponse {
                        request_id,
                        answer_result: IpcResult::Error(IpcErrorPayload {
                            code: IpcErrorCode::MalformedRequest,
                            message: restart_error_message,
                        }),
                    },
                    // No dispatcher left: the connection closes with no
                    // answer.
                    None => return,
                }
            }
            // No answer belongs to this one.
            IpcRequestKind::Leaving => return,
        };
        if connection.send(&outgoing_response).is_err() {
            return;
        }
    }
}

/// Carry `client_id`'s event stream and input on its own connection until the
/// peer goes away, then detach the client.
///
/// The connection is split: a spawned thread drains the client's queue and
/// writes one frame per delivery that says something about the session's
/// structure, while this thread reads the client's own frames. A `Keyboard`, a
/// `Resize`, a `Paste`, a `SubmitCommand` and a `Mouse` round all cross to the
/// dispatcher over the inbox, and this half writes nothing back for any of
/// them: keyboard, resize, and paste requests are reflected by the next
/// painted frame; a submitted command is reflected by the next painted frame
/// or, for a rejected pane placement command, one
/// [`SessionEvent::PlacementCommandRejected`] carrying its `command_id`; and
/// a `Mouse` round is answered on the writing half by exactly one
/// [`SessionEvent::MouseAnswer`] carrying that round's `request_id`. A request
/// of any other kind, end of stream, a transport fault, an oversize frame, or a
/// dispatcher that is gone all end the reading loop.
///
/// A frame that arrives whole and does not decode costs the client that one
/// request. The reading half drops it and reads the next frame, and the client
/// stays attached. A request kind this build does not have is dropped the same
/// way. Nothing is written back for either: this half writes no frames.
///
/// A `SubmitCommand` on this connection has its source stamped by
/// [`stamp_client_command_source`]: it is attributed to `client_id` and to no
/// other client.
///
/// Either half ending detaches the client, which removes its record and drops
/// its subscription; the closed queue, or the terminal `Quit` or `Restarting`
/// frame, then ends the writing thread. Both halves hand over a detach when
/// they end. A detach for a client already gone changes nothing.
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
/// started the session. It is read after each read on this connection,
/// whether or not the frame decoded, and turning the setting off detaches that
/// client at its next frame.
///
/// This connection's place in the intake's attached count is held by the
/// caller: taken before the `Attached` frame is written, dropped once this
/// returns.
///
/// `served_connection` is this connection's entry in the [`Intake`]: the intake holds this
/// connection's read direction through it. The client's record stays as it is:
/// the image swap that cut the connection carries it across.
fn stream_events(
    connection: Connection,
    attached_stream: AttachedStream,
    inbox_sender: &Sender<RuntimeEvent>,
    live_setting: Option<OtherUsersSetting>,
    served_connection: &ServedConnection,
) {
    let AttachedStream {
        client_id,
        deliveries,
        ending_notice,
        graphics_capabilities,
    } = attached_stream;
    let (mut frame_reader, mut frame_writer) = connection.split();
    let writer_inbox_sender = inbox_sender.clone();
    let writer_intake = Arc::clone(&served_connection.intake);
    ending_notice.record_writer_started();
    std::thread::spawn(move || {
        let mut image_cache = ConnectionImageCache::new();
        loop {
            // The session is ending. This client is told at once and whatever
            // is still queued for it goes unwritten. A queue the server already
            // closed detached this client before the session ended: its own
            // goodbye is what it reads.
            if let Some(session_ending) = ending_notice.get_session_ending() {
                let terminal_event = loop {
                    match deliveries.try_recv() {
                        Ok(_) => {}
                        Err(mpsc::TryRecvError::Empty) => {
                            break match session_ending {
                                SessionEnding::Quit => SessionEvent::Quit,
                                SessionEnding::Restarting => SessionEvent::Restarting,
                            }
                        }
                        Err(mpsc::TryRecvError::Disconnected) => break SessionEvent::Detached,
                    }
                };
                let _ = frame_writer.send(&terminal_event);
                break;
            }
            let Ok(delivery) = deliveries.recv() else {
                // The queue closed, which the server does when it detaches this
                // client. `recv` hands back everything queued before the close:
                // the goodbye follows the events that preceded it.
                let _ = frame_writer.send(&SessionEvent::Detached);
                break;
            };
            let has_write_failed = match &delivery {
                Delivery::Frame(render_snapshot) => send_painted_frame(
                    &mut frame_writer,
                    &mut image_cache,
                    render_snapshot,
                    graphics_capabilities,
                    client_id,
                ),
                Delivery::PanePlacementSnapshot {
                    request_id,
                    snapshot: placement_snapshot,
                } => send_placement_snapshot(
                    &mut frame_writer,
                    &mut image_cache,
                    *request_id,
                    placement_snapshot,
                    graphics_capabilities,
                    client_id,
                ),
                _ => false,
            };
            if has_write_failed {
                break;
            }
            if matches!(
                delivery,
                Delivery::Frame(_) | Delivery::PanePlacementSnapshot { .. }
            ) {
                continue;
            }
            let session_event = build_wire_event(&delivery);
            if let Some(session_event) = session_event {
                let has_write_failed = match frame_writer.send(&session_event) {
                    Ok(()) => false,
                    Err(IpcError::FrameTooLarge {
                        frame_byte_count,
                        maximum_frame_byte_count,
                    }) => {
                        tracing::warn!(
                            %client_id,
                            frame_byte_count,
                            maximum_frame_byte_count,
                            "frame over the cap was not sent"
                        );
                        false
                    }
                    Err(_) => true,
                };
                if has_write_failed {
                    break;
                }
                // `Quit` and `Restarting` are the stream's terminal frames; the
                // loop ends on either without waiting for the queue to close.
                if matches!(session_event, SessionEvent::Quit | SessionEvent::Restarting) {
                    break;
                }
            }
        }
        writer_intake.hand_over_event(
            &writer_inbox_sender,
            RuntimeEvent::ClientDetached {
                client_id,
                detached_at: SystemTime::now(),
                is_streamed: true,
            },
        );
        ending_notice.record_writer_ended();
    });

    loop {
        let incoming_request_result = frame_reader.recv::<IncomingRequest>();
        // The setting is read again after each read on this connection,
        // whether or not the frame decoded. A client whose access was
        // withdrawn reads no further frame of this session.
        if live_setting
            .as_ref()
            .is_some_and(|is_enabled| !is_enabled())
        {
            break;
        }
        let incoming_request = match incoming_request_result {
            Ok(incoming_request) => incoming_request,
            // The frame arrived whole and its bytes did not decode.
            // `read_message` consumed exactly that frame, and the next read
            // starts on a frame boundary. This one request is dropped, the
            // loop reads the next frame, and the client keeps its stream. The
            // line carries the client id and none of the peer's bytes.
            Err(IpcError::MalformedFrame { .. }) => {
                tracing::debug!(%client_id, "frame this build cannot read");
                continue;
            }
            // An oversize frame leaves its payload unread, and a disconnect
            // or a transport fault leaves no stream. All three end this loop.
            Err(_) => break,
        };
        let request_kind = match incoming_request.request_kind {
            MaybeKnown::Known(request_kind) => request_kind,
            // A kind this build does not have comes from a newer koshi. The
            // one request is dropped and the client keeps its stream.
            MaybeKnown::Unknown { variant_name } => {
                tracing::debug!(%client_id, %variant_name, "request kind this build does not have");
                continue;
            }
        };
        let runtime_event = match request_kind {
            IpcRequestKind::CellSize { cell_size } => RuntimeEvent::CellSize {
                client_id,
                cell_size,
            },
            IpcRequestKind::Keyboard { key_input } => RuntimeEvent::ClientKeyboard {
                client_id,
                key_input,
            },
            IpcRequestKind::Resize {
                viewport_size,
                pane_area,
                cell_size,
            } => RuntimeEvent::Resize {
                client_id,
                viewport_size,
                pane_area,
                cell_size,
            },
            IpcRequestKind::Paste { pasted_text } => RuntimeEvent::HostPaste {
                client_id,
                pasted_text,
            },
            IpcRequestKind::Mouse(mouse_actions) => RuntimeEvent::ClientMouse {
                client_id,
                request_id: incoming_request.request_id,
                mouse_actions,
            },
            IpcRequestKind::SubmitCommand(command_envelope) => {
                // The result is answered by the next painted frame. The reply
                // channel's receiving end drops here, and the dispatcher's send
                // into it fails.
                let (response_sender, _) = mpsc::channel();
                RuntimeEvent::Ipc {
                    command_envelope: Box::new(stamp_client_command_source(
                        *command_envelope,
                        client_id,
                    )),
                    response_sender,
                }
            }
            IpcRequestKind::ReadPanePlacement {
                pane_id,
                destination_tab_id,
            } => RuntimeEvent::ReadPanePlacement {
                client_id,
                request_id: incoming_request.request_id,
                source_pane_id: pane_id,
                destination_tab_id,
            },
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
        if !served_connection
            .intake
            .hand_over_event(inbox_sender, runtime_event)
        {
            break;
        }
    }
    served_connection.intake.hand_over_event(
        inbox_sender,
        RuntimeEvent::ClientDetached {
            client_id,
            detached_at: SystemTime::now(),
            is_streamed: true,
        },
    );
}

/// One image record retained while its placement remains in this connection's frame.
struct CachedConnectionImage {
    /// Identity used by painted frames and image transfer events.
    image_content_id: u64,
    /// The image record uploaded for this identity. `None` remains a placeholder.
    image_record: Option<Arc<ImageRecord>>,
}

/// Image identities and records that belong to one attached connection.
struct ConnectionImageCache {
    /// Current records, keyed by pane and terminal-local placement identity.
    cached_image_by_pane_and_placement_id: HashMap<(PaneId, u64), CachedConnectionImage>,
    /// The next nonzero connection-local content identity.
    next_image_content_id: u64,
    /// Whether the next successful frame must invalidate the client's records.
    needs_image_cache_reset: bool,
}

impl ConnectionImageCache {
    /// Build an empty cache whose first image identity is 1.
    fn new() -> Self {
        Self {
            cached_image_by_pane_and_placement_id: HashMap::new(),
            next_image_content_id: 1,
            needs_image_cache_reset: false,
        }
    }

    /// Assign stable content identities and list records this connection has not received.
    fn prepare_image_frame(&mut self, render_snapshot: &RenderSnapshot) -> PreparedImageFrame {
        let image_placements: Vec<((PaneId, u64), &ImagePlacementSnapshot)> = render_snapshot
            .pane_snapshots
            .iter()
            .flat_map(|pane_snapshot| {
                pane_snapshot.image_placement_snapshots.iter().map(
                    move |image_placement_snapshot| {
                        (
                            (
                                pane_snapshot.pane_id,
                                image_placement_snapshot.get_placement_id(),
                            ),
                            image_placement_snapshot,
                        )
                    },
                )
            })
            .collect();
        let (should_reset_image_cache, image_uploads) =
            self.prepare_image_records(image_placements);

        let painted_frame =
            build_wire_frame_with_content_ids(render_snapshot, |pane_id, image_placement| {
                self.find_image_content_id(pane_id, image_placement)
            });
        PreparedImageFrame {
            should_reset_image_cache,
            painted_frame,
            image_uploads,
        }
    }

    /// Assign stable content identities and list image records not sent on this connection.
    fn prepare_image_records(
        &mut self,
        image_placements: Vec<((PaneId, u64), &ImagePlacementSnapshot)>,
    ) -> (bool, Vec<(u64, Arc<ImageRecord>)>) {
        let changed_image_count = image_placements
            .iter()
            .filter(|(placement_key, image_placement)| {
                self.cached_image_by_pane_and_placement_id
                    .get(placement_key)
                    .is_none_or(|cached_image| {
                        !is_same_image_record(
                            cached_image.image_record.as_ref(),
                            image_placement.get_image_record(),
                        )
                    })
            })
            .count();
        let available_image_content_count = if self.next_image_content_id == 0 {
            0
        } else {
            u64::MAX - self.next_image_content_id + 1
        };
        let should_reset_image_cache = self.needs_image_cache_reset
            || u64::try_from(changed_image_count).map_or(true, |image_count| {
                image_count > available_image_content_count
            });
        if should_reset_image_cache {
            self.cached_image_by_pane_and_placement_id.clear();
            self.next_image_content_id = 1;
            self.needs_image_cache_reset = false;
        }

        let mut image_uploads = Vec::new();
        let mut retained_placement_keys = HashSet::with_capacity(image_placements.len());
        let mut image_content_id_by_memory_address = self
            .cached_image_by_pane_and_placement_id
            .values()
            .filter_map(|cached_image| {
                cached_image.image_record.as_ref().map(|image_record| {
                    (
                        Arc::as_ptr(&image_record.image),
                        cached_image.image_content_id,
                    )
                })
            })
            .collect::<HashMap<_, _>>();
        for (placement_key, image_placement) in image_placements {
            retained_placement_keys.insert(placement_key);
            let image_record = image_placement.clone_image_record();
            let is_unchanged = self
                .cached_image_by_pane_and_placement_id
                .get(&placement_key)
                .is_some_and(|cached_image| {
                    is_same_image_record(
                        cached_image.image_record.as_ref(),
                        image_record.as_deref(),
                    )
                });
            if is_unchanged {
                continue;
            }
            let image_content_id = image_record
                .as_ref()
                .and_then(|image_record| {
                    image_content_id_by_memory_address
                        .get(&Arc::as_ptr(&image_record.image))
                        .copied()
                })
                .unwrap_or_else(|| {
                    let image_content_id = self.next_image_content_id;
                    self.next_image_content_id =
                        self.next_image_content_id.checked_add(1).unwrap_or(0);
                    if let Some(image_record) = image_record.as_ref() {
                        image_content_id_by_memory_address
                            .insert(Arc::as_ptr(&image_record.image), image_content_id);
                        image_uploads.push((image_content_id, Arc::clone(image_record)));
                    }
                    image_content_id
                });
            self.cached_image_by_pane_and_placement_id.insert(
                placement_key,
                CachedConnectionImage {
                    image_content_id,
                    image_record,
                },
            );
        }
        self.cached_image_by_pane_and_placement_id
            .retain(|placement_key, _| retained_placement_keys.contains(placement_key));
        (should_reset_image_cache, image_uploads)
    }

    /// Prepare a placement preview and its connection-local image transfers.
    fn prepare_placement_snapshot(
        &mut self,
        placement_snapshot: &PlacementSnapshot,
    ) -> PreparedPlacementSnapshot {
        let image_placements = [
            Some(&placement_snapshot.source_tab_snapshot),
            placement_snapshot.destination_tab_snapshot.as_ref(),
        ]
        .into_iter()
        .flatten()
        .flat_map(|tab_snapshot| {
            tab_snapshot
                .pane_snapshots
                .iter()
                .flat_map(|pane_snapshot| {
                    pane_snapshot.image_placement_snapshots.iter().map(
                        move |image_placement_snapshot| {
                            (
                                (
                                    pane_snapshot.pane_id,
                                    image_placement_snapshot.get_placement_id(),
                                ),
                                image_placement_snapshot,
                            )
                        },
                    )
                })
        })
        .collect();
        let (should_reset_image_cache, image_uploads) =
            self.prepare_image_records(image_placements);
        let placement_snapshot =
            crate::runtime::frame::build_wire_placement_snapshot_with_content_ids(
                placement_snapshot,
                |pane_id, image_placement| self.find_image_content_id(pane_id, image_placement),
            );
        PreparedPlacementSnapshot {
            should_reset_image_cache,
            placement_snapshot,
            image_uploads,
        }
    }

    /// The connection-local content identity cached for `image_placement` in
    /// `pane_id`, or the placement's own content identity when this connection
    /// caches none for it.
    fn find_image_content_id(
        &self,
        pane_id: PaneId,
        image_placement: &ImagePlacementSnapshot,
    ) -> u64 {
        self.cached_image_by_pane_and_placement_id
            .get(&(pane_id, image_placement.get_placement_id()))
            .map_or(image_placement.get_image_content_id(), |cached_image| {
                cached_image.image_content_id
            })
    }

    /// Forget every connection-local image identity, start identities at 1
    /// again, and mark the next frame to reset the client's image records.
    fn clear_image_cache(&mut self) {
        self.cached_image_by_pane_and_placement_id.clear();
        self.next_image_content_id = 1;
        self.needs_image_cache_reset = true;
    }
}

/// A painted frame plus the image records its connection still needs.
struct PreparedImageFrame {
    /// Whether the client must discard every record before this frame.
    should_reset_image_cache: bool,
    /// Placement geometry and connection-local content identities.
    painted_frame: PaintedFrame,
    /// New content identities and the records uploaded under them.
    image_uploads: Vec<(u64, Arc<ImageRecord>)>,
}

/// A placement preview plus the image records its connection still needs.
struct PreparedPlacementSnapshot {
    /// Whether the client must discard every record before this preview.
    should_reset_image_cache: bool,
    /// The preview with connection-local image identities.
    placement_snapshot: koshi_ipc::placement::PanePlacementSnapshot,
    /// New content identities and the records uploaded under them.
    image_uploads: Vec<(u64, Arc<ImageRecord>)>,
}

/// Report whether two cached record slots hold the same retained image record.
fn is_same_image_record(
    cached_image_record: Option<&Arc<ImageRecord>>,
    incoming_image_record: Option<&ImageRecord>,
) -> bool {
    match (cached_image_record, incoming_image_record) {
        (Some(cached_image_record), Some(incoming_image_record)) => {
            Arc::ptr_eq(&cached_image_record.image, &incoming_image_record.image)
        }
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

/// Send one painted frame and each new image record after it.
///
/// A client whose terminal has no native image protocol receives every image
/// placement through [`hide_image_records`], and no image record.
///
/// Returns `true` when the connection is no longer usable.
fn send_painted_frame(
    frame_writer: &mut FrameWriter,
    image_cache: &mut ConnectionImageCache,
    render_snapshot: &RenderSnapshot,
    graphics_capabilities: GraphicsCapabilities,
    client_id: ClientId,
) -> bool {
    if !graphics_capabilities.has_native_image_protocol() {
        let mut painted_frame = build_wire_frame(render_snapshot);
        for pane_snapshot in &mut painted_frame.pane_snapshots {
            hide_image_records(&mut pane_snapshot.image_placement_snapshots);
        }
        let painted_frame_event = SessionEvent::Painted {
            frame: Box::new(painted_frame),
        };
        return match frame_writer.send(&painted_frame_event) {
            Ok(()) => false,
            Err(write_error) => {
                report_image_send_error(image_cache, client_id, write_error, "the painted frame")
            }
        };
    }
    let prepared_image_frame = image_cache.prepare_image_frame(render_snapshot);
    if prepared_image_frame.should_reset_image_cache {
        if let Err(write_error) = frame_writer.send(&SessionEvent::ImageCacheReset) {
            return report_image_send_error(
                image_cache,
                client_id,
                write_error,
                "the image cache reset",
            );
        }
    }
    let painted_frame_event = SessionEvent::Painted {
        frame: Box::new(prepared_image_frame.painted_frame),
    };
    send_image_uploads(
        frame_writer,
        image_cache,
        client_id,
        &painted_frame_event,
        "the painted frame",
        prepared_image_frame.image_uploads,
    )
}

/// Send one placement preview and each new image record after it.
///
/// A client whose terminal has no native image protocol receives every image
/// placement through [`hide_image_records`], and no image record.
///
/// Returns `true` when the connection is no longer usable.
fn send_placement_snapshot(
    frame_writer: &mut FrameWriter,
    image_cache: &mut ConnectionImageCache,
    request_id: u64,
    placement_snapshot: &PlacementSnapshot,
    graphics_capabilities: GraphicsCapabilities,
    client_id: ClientId,
) -> bool {
    if !graphics_capabilities.has_native_image_protocol() {
        let mut wire_placement_snapshot = build_wire_placement_snapshot(placement_snapshot);
        hide_placement_image_records(&mut wire_placement_snapshot);
        let placement_snapshot_event = SessionEvent::PanePlacementSnapshot {
            request_id,
            snapshot: Box::new(wire_placement_snapshot),
        };
        return match frame_writer.send(&placement_snapshot_event) {
            Ok(()) => false,
            Err(write_error) => report_image_send_error(
                image_cache,
                client_id,
                write_error,
                "the placement snapshot",
            ),
        };
    }
    let prepared_placement_snapshot = image_cache.prepare_placement_snapshot(placement_snapshot);
    if prepared_placement_snapshot.should_reset_image_cache {
        if let Err(write_error) = frame_writer.send(&SessionEvent::ImageCacheReset) {
            return report_image_send_error(
                image_cache,
                client_id,
                write_error,
                "the placement image cache reset",
            );
        }
    }
    let placement_snapshot_event = SessionEvent::PanePlacementSnapshot {
        request_id,
        snapshot: Box::new(prepared_placement_snapshot.placement_snapshot),
    };
    send_image_uploads(
        frame_writer,
        image_cache,
        client_id,
        &placement_snapshot_event,
        "the placement snapshot",
        prepared_placement_snapshot.image_uploads,
    )
}

/// Send `leading_event`, then each record in `image_uploads` as one
/// `ImageContentStart` followed by its `ImageContentChunk` frames.
/// `leading_event` is sent again before every
/// `MAX_FRAME_IMAGE_TRANSFER_COUNT`-th record: 9 records with a limit of 4 send
/// the event, records 0-3, the event, records 4-7, the event, record 8.
/// `leading_event_part` names `leading_event` in the warning a failed write
/// logs, and a failed record write logs `an image transfer start after
/// <leading_event_part>` or `an image transfer chunk after <leading_event_part>`.
/// Returns `true` when the connection is no longer usable.
fn send_image_uploads(
    frame_writer: &mut FrameWriter,
    image_cache: &mut ConnectionImageCache,
    client_id: ClientId,
    leading_event: &SessionEvent,
    leading_event_part: &str,
    image_uploads: Vec<(u64, Arc<ImageRecord>)>,
) -> bool {
    if let Err(write_error) = frame_writer.send(leading_event) {
        return report_image_send_error(image_cache, client_id, write_error, leading_event_part);
    }
    for (upload_index, (image_content_id, image_record)) in image_uploads.into_iter().enumerate() {
        if upload_index != 0 && upload_index % MAX_FRAME_IMAGE_TRANSFER_COUNT == 0 {
            if let Err(write_error) = frame_writer.send(leading_event) {
                return report_image_send_error(
                    image_cache,
                    client_id,
                    write_error,
                    leading_event_part,
                );
            }
        }
        let image_transfer_start_event = SessionEvent::ImageContentStart {
            image_transfer: build_wire_image_transfer(image_content_id, &image_record),
        };
        if let Err(write_error) = frame_writer.send(&image_transfer_start_event) {
            return report_image_send_error(
                image_cache,
                client_id,
                write_error,
                &format!("an image transfer start after {leading_event_part}"),
            );
        }
        for (byte_offset, is_last_chunk, chunk_bytes) in
            build_wire_image_chunk_sources(&image_record)
        {
            let image_transfer_chunk_event = SessionEvent::ImageContentChunk {
                image_chunk: FrameImageChunk {
                    image_transfer_id: image_content_id,
                    byte_offset,
                    is_last: is_last_chunk,
                    chunk_bytes: chunk_bytes.to_vec(),
                },
            };
            if let Err(write_error) = frame_writer.send(&image_transfer_chunk_event) {
                return report_image_send_error(
                    image_cache,
                    client_id,
                    write_error,
                    &format!("an image transfer chunk after {leading_event_part}"),
                );
            }
        }
    }
    false
}

/// Run [`hide_image_records`] on every pane of the source tab and of the
/// destination tab, when there is one, in `placement_snapshot`.
fn hide_placement_image_records(
    placement_snapshot: &mut koshi_ipc::placement::PanePlacementSnapshot,
) {
    for placement_tab_snapshot in [
        Some(&mut placement_snapshot.source_tab_snapshot),
        placement_snapshot.destination_tab_snapshot.as_mut(),
    ]
    .into_iter()
    .flatten()
    {
        for pane_snapshot in &mut placement_tab_snapshot.pane_snapshots {
            hide_image_records(&mut pane_snapshot.image_placement_snapshots);
        }
    }
}

/// Clear `image_record` and set `is_available` to `false` on every placement in
/// `image_placements`. A client paints such a placement as the
/// `terminal image unavailable` placeholder and waits for no image content.
fn hide_image_records(image_placements: &mut [FrameImagePlacement]) {
    for image_placement in image_placements {
        image_placement.image_record = None;
        image_placement.is_available = false;
    }
}

/// Log one image-stream write failure. A frame over the cap clears
/// `image_cache` and returns `false`: the connection stays usable. Any other
/// failure returns `true`: the connection is no longer usable.
fn report_image_send_error(
    image_cache: &mut ConnectionImageCache,
    client_id: ClientId,
    write_error: IpcError,
    failed_image_transfer_part: &str,
) -> bool {
    match write_error {
        IpcError::FrameTooLarge {
            frame_byte_count,
            maximum_frame_byte_count: frame_byte_limit,
        } => {
            tracing::warn!(
                %client_id,
                failed_image_transfer_part,
                frame_byte_count,
                frame_byte_limit,
                "image transfer part exceeded the frame cap"
            );
            image_cache.clear_image_cache();
            false
        }
        write_error => {
            tracing::warn!(
                %client_id,
                failed_image_transfer_part,
                %write_error,
                "image transfer write failed"
            );
            true
        }
    }
}

/// Rebuild `command_envelope` with the source a control connection carries,
/// over whatever source its sender wrote.
///
/// A control connection carries a `koshi` CLI invocation. Its two sources,
/// [`CommandSource::InSessionCli`] and [`CommandSource::ExternalCli`], are kept
/// as they stand. Every other source becomes
/// `ExternalCli { session_id: None, target_client_id: None }`, which names no
/// session and no client.
///
/// The envelope's `client_id` is re-derived from the stamped source; the two
/// always agree.
///
/// A sender that writes `CommandSource::KeyBinding` and
/// `Command::ToggleMouseSelect` reaches the dispatcher as
/// `ExternalCli { session_id: None, target_client_id: None }` carrying
/// `Command::ToggleMouseSelect`. The dispatcher's CLI-admission check refuses
/// it: the CLI has no mouse-select verb.
fn stamp_cli_command_source(command_envelope: CommandEnvelope) -> CommandEnvelope {
    let command_source = match command_envelope.command_source {
        command_source @ (CommandSource::InSessionCli { .. }
        | CommandSource::ExternalCli { .. }) => command_source,
        CommandSource::KeyBinding { .. } | CommandSource::Mouse { .. } => {
            CommandSource::from_external_cli(None, None)
        }
    };
    CommandEnvelope::from_parts(
        command_envelope.command_id,
        command_source,
        command_envelope.command,
    )
}

/// Rebuild `command_envelope` with [`CommandSource::KeyBinding`] naming
/// `client_id`, over whatever source its sender wrote.
///
/// `client_id` is the client this connection attached as. A command this
/// connection sends is attributed to that client and to no other.
///
/// The envelope's `client_id` is re-derived from the stamped source; the two
/// always agree.
fn stamp_client_command_source(
    command_envelope: CommandEnvelope,
    client_id: ClientId,
) -> CommandEnvelope {
    CommandEnvelope::from_parts(
        command_envelope.command_id,
        CommandSource::from_key_binding(client_id),
        command_envelope.command,
    )
}

/// Hand one request to the dispatcher thread and wait for its answer: build
/// the inbox event around a fresh reply channel, hand it over the intake, and
/// block on the reply. `None` means no answer is coming: the intake is closed,
/// or the dispatcher is gone.
fn request_dispatcher_response<DispatcherResponse>(
    connection_intake: &Intake,
    inbox_sender: &Sender<RuntimeEvent>,
    build_runtime_event: impl FnOnce(mpsc::Sender<DispatcherResponse>) -> RuntimeEvent,
) -> Option<DispatcherResponse> {
    let (response_sender, response_receiver) = mpsc::channel();
    if !connection_intake.hand_over_event(inbox_sender, build_runtime_event(response_sender)) {
        return None;
    }
    response_receiver.recv().ok()
}

#[cfg(test)]
mod tests;
