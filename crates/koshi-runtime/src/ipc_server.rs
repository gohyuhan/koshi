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
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{CommandEnvelope, CommandSource};
use koshi_core::constant::{compute_heartbeat_interval, MIN_LINK_TIMEOUT_MS};
use koshi_core::ids::{ClientId, PaneId, SessionId};
use koshi_ipc::endpoint::{
    compute_socket_address, delete_advertisement_marker, delete_socket_file,
    resolve_advertisement_marker_path, write_advertisement_marker, EndpointFile, ServerProgramFile,
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

use crate::executable_watch::ExecutableWatch;
use crate::runtime::bus::{build_wire_event, build_wire_host_write_events, WaitingHostWrites};
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

    /// Close the read direction of the connection filed under
    /// `connection_number`, as [`ReadCloser::close`] states. A number that no
    /// served connection is filed under changes nothing.
    fn close_read_direction(&self, connection_number: u64) {
        let intake_state = self.intake_state.read().expect("intake");
        if let Some(reader) = intake_state
            .reader_by_connection_number
            .get(&connection_number)
        {
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
    /// The host-write bytes that wait for the client's own terminal, which the
    /// writer takes before each delivery and before its last frame.
    waiting_host_writes: Arc<WaitingHostWrites>,
    /// Shared session-ending notice read by the writer.
    ending_notice: Arc<EndingNotice>,
    /// Graphics capabilities reported by this connection.
    graphics_capabilities: GraphicsCapabilities,
    /// How long the writing thread lets the connection go without a frame
    /// before it writes a [`SessionEvent::Pong`]: a third of the Attach
    /// request's `link_timeout_ms`. `None` writes no `Pong`.
    heartbeat_interval: Option<Duration>,
    /// Whether the connection came from another machine. A connection that
    /// ends with no `Leaving` request, after the reading loop read its first
    /// request, reports its client as returning when this is `true`.
    is_remote: bool,
    /// Whether a connection that ends with no `Leaving` request, before the
    /// reading loop reads its first request, reports its client as returning:
    /// `true` when the connection came from another machine, or when its
    /// Attach named a client to come back as (`resume_client_id`).
    is_returning_before_first_request: bool,
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
    /// The program file naming the koshi version and program file this
    /// session runs. `None` for a session served with no executable watch,
    /// which writes no program file.
    program_file_path: Option<PathBuf>,
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
    ///
    /// `executable_watch` `Some` checks the program file the session started
    /// from on each connection the socket takes, as
    /// [`ExecutableWatch::check_executable_file`] states. A file that now
    /// holds another koshi version sends the dispatcher
    /// [`RuntimeEvent::IpcRestart`], as a `Restart` request does. The session
    /// also writes its [`ServerProgramFile`] after the endpoint file, from the
    /// watch's path and running version, and a failed write unwinds the bind
    /// as a failed endpoint write does.
    pub fn start(
        runtime_directory: &Path,
        session_id: SessionId,
        inbox_sender: Sender<RuntimeEvent>,
        other_users: Option<OtherUsers>,
        executable_watch: Option<Arc<ExecutableWatch>>,
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
                delete_socket_file(&socket_address);
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
        let program_file_path = executable_watch.as_ref().map(|_| {
            ServerProgramFile::resolve_session_program_file_path(runtime_directory, session_id)
        });
        let advertisement_result = endpoint_file
            .write_to_path(&endpoint_path)
            .and_then(|()| match (&executable_watch, &program_file_path) {
                (Some(executable_watch), Some(program_file_path)) => executable_watch
                    .build_server_program_file()
                    .write_to_path(program_file_path),
                _ => Ok(()),
            })
            .and_then(|()| match &shared_socket_marker_path {
                None => Ok(()),
                Some(shared_socket_marker_path) => {
                    write_advertisement_marker(shared_socket_marker_path)
                }
            });
        if let Err(advertisement_error) = advertisement_result {
            // Dropping the listener releases the address and unlinks the socket
            // file on Unix. The endpoint write is atomic: a file left at
            // `endpoint_path` is an older run's, naming the socket removed here.
            if let Some(program_file_path) = &program_file_path {
                let _ = std::fs::remove_file(program_file_path);
            }
            let _ = std::fs::remove_file(&endpoint_path);
            drop(listener);
            delete_socket_file(&socket_address);
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
                executable_watch.as_ref(),
            );
        });

        Ok(IpcServer {
            socket_address,
            endpoint_path,
            program_file_path,
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
    /// joined, and the program file, the endpoint file, the socket and any
    /// shared marker are removed, in that order. Connections already being
    /// served run out on their own threads; with the dispatcher draining, their
    /// in-flight requests end in a closed connection rather than a mutation.
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
        if let Some(program_file_path) = &self.program_file_path {
            let _ = std::fs::remove_file(program_file_path);
        }
        let _ = std::fs::remove_file(&self.endpoint_path);
        if let Some(shared_socket_marker_path) = &self.shared_socket_marker_path {
            delete_advertisement_marker(shared_socket_marker_path);
        }
        delete_socket_file(&self.socket_address);
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
///
/// Each connection the intake takes runs a check of `executable_watch`, and
/// its serving thread runs
/// [`ExecutableWatch::check_executable_file_after_refused_hello`] for each
/// Hello refused for its protocol version. When either finds another koshi
/// version, [`restart_into_installed_version`] asks the dispatcher to restart.
fn run_accept_loop(
    listener: &Listener,
    connection_token: &RwLock<ConnectionToken>,
    inbox_sender: &Sender<RuntimeEvent>,
    is_shutting_down: &AtomicBool,
    allow_other_users_setting: Option<&OtherUsersSetting>,
    connection_intake: &Arc<Intake>,
    executable_watch: Option<&Arc<ExecutableWatch>>,
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
            if let Some(executable_watch) = executable_watch {
                executable_watch.check_executable_file(build_installed_version_restart(
                    connection_intake,
                    inbox_sender,
                    executable_watch,
                ));
            }
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
            let executable_watch = executable_watch.cloned();
            std::thread::spawn(move || {
                serve_connection(
                    connection,
                    connection_token,
                    &inbox_sender,
                    peer,
                    live_setting,
                    &served_connection,
                    executable_watch.as_ref(),
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
/// `Attach`, `Discovery`, `Layout`, `Restart`, `ClipboardCopyPart` and
/// `ClipboardCopy` cross to the dispatcher over the inbox and answer with its
/// reply. `RecentEvents` is answered on this thread, from the process-wide log
/// ring, and reaches no dispatcher.
///
/// A clipboard copy that a stand-in clipboard tool sends is held on this
/// connection part by part:
///
/// 1. Each `ClipboardCopyPart` asks the dispatcher whether the session takes a
///    copy of every byte held plus the part's bytes
///    ([`Server::can_take_clipboard_tool_copy`](crate::server::Server::can_take_clipboard_tool_copy)).
///    If yes, the connection holds the part and answers
///    `ClipboardCopyPartTaken`. If no, the connection drops every byte it
///    holds and answers `ClipboardCopyRouted` with `should_run_clipboard_tool`
///    set to `true`.
/// 2. A `ClipboardCopy` hands the dispatcher the held bytes, then its own
///    bytes, as one copy
///    ([`Server::route_clipboard_tool_copy`](crate::server::Server::route_clipboard_tool_copy)),
///    and answers `ClipboardCopyRouted` with the dispatcher's answer. The
///    connection then holds no bytes.
///
/// Both kinds are refused with [`IpcErrorCode::RequestFailed`] on a connection
/// from another machine or from another local user, and the connection keeps
/// serving.
///
/// A `SubmitCommand` on this connection has its source stamped by
/// [`stamp_cli_command_source`] before it crosses to the dispatcher: this connection carries
/// a `koshi` CLI invocation.
///
/// A `Restart` the dispatcher refuses is answered with
/// [`IpcErrorCode::RequestFailed`] carrying the sentence naming what is
/// wrong, and the connection keeps serving.
///
/// An answered `Attach` ends the request loop: the connection is handed to
/// [`stream_events`], which carries that client's events out and that client's
/// input in for as long as the connection lives. An `Attached` answer that
/// cannot be written detaches the client: as returning when the connection
/// came from another machine or the `Attach` names a client to come back as
/// (`resume_client_id`), and as not returning otherwise. An `Attach` whose
/// `link_timeout_ms` is below [`MIN_LINK_TIMEOUT_MS`], `1000`, reaches no
/// dispatcher: it is refused with [`IpcErrorCode::RequestFailed`] and the
/// sentence `a link timeout of 999 ms is below the minimum of 1000 ms` for
/// `999`, and the connection keeps serving.
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
///
/// A Hello refused for its protocol version runs
/// [`ExecutableWatch::check_executable_file_after_refused_hello`] on
/// `executable_watch`, which restarts the session through
/// [`restart_into_installed_version`] when the program file holds another
/// koshi version, and the connection keeps serving.
fn serve_connection(
    mut connection: Connection,
    connection_token: ConnectionToken,
    inbox_sender: &Sender<RuntimeEvent>,
    peer: Peer,
    live_setting: Option<OtherUsersSetting>,
    served_connection: &ServedConnection,
    executable_watch: Option<&Arc<ExecutableWatch>>,
) {
    let mut handshake = Handshake::from_expected_token_and_peer(connection_token, peer);
    let mut held_copy_bytes: Vec<u8> = Vec::new();
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
            RequestDisposition::VersionRefused => {
                if let Some(executable_watch) = executable_watch {
                    executable_watch.check_executable_file_after_refused_hello(
                        build_installed_version_restart(
                            &served_connection.intake,
                            inbox_sender,
                            executable_watch,
                        ),
                    );
                }
                continue;
            }
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
                link_timeout_ms: Some(link_timeout_ms),
                ..
            } if link_timeout_ms < MIN_LINK_TIMEOUT_MS => IpcResponse {
                request_id,
                answer_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::RequestFailed,
                    message: format!(
                        "a link timeout of {link_timeout_ms} ms is below the minimum of \
                         {MIN_LINK_TIMEOUT_MS} ms"
                    ),
                }),
            },
            IpcRequestKind::Attach {
                viewport_size,
                resume_client_id,
                resume_token,
                pane_area,
                graphics_capabilities,
                cell_size,
                link_timeout_ms,
            } => {
                // A connection that ends before its first request after this
                // attach reports its client as returning when it came from
                // another machine, or when the attach names a client to come
                // back as.
                let is_returning_before_first_request =
                    handshake.is_remote_caller() || resume_client_id.is_some();
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
                            is_returning: is_returning_before_first_request,
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
                        waiting_host_writes: accepted_client.waiting_host_writes,
                        ending_notice: accepted_client.ending_notice,
                        graphics_capabilities,
                        heartbeat_interval: link_timeout_ms.map(compute_heartbeat_interval),
                        is_remote: handshake.is_remote_caller(),
                        is_returning_before_first_request,
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
            | IpcRequestKind::ReadPanePlacement { .. }
            | IpcRequestKind::Ping => return,
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
                    Some(Err(restart_refusal)) => IpcResponse {
                        request_id,
                        answer_result: IpcResult::Error(IpcErrorPayload {
                            code: IpcErrorCode::RequestFailed,
                            message: restart_refusal.to_string(),
                        }),
                    },
                    // No dispatcher left: the connection closes with no
                    // answer.
                    None => return,
                }
            }
            // No answer belongs to this one.
            IpcRequestKind::Leaving { .. } => return,
            IpcRequestKind::ClipboardCopyPart { .. } | IpcRequestKind::ClipboardCopy { .. }
                if handshake.is_remote_caller() || live_setting.is_some() =>
            {
                IpcResponse {
                    request_id,
                    answer_result: IpcResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::RequestFailed,
                        message: "a pane clipboard copy must come from this machine and this user"
                            .to_owned(),
                    }),
                }
            }
            IpcRequestKind::ClipboardCopyPart { copied_bytes } => {
                let held_copy_byte_count = held_copy_bytes.len() + copied_bytes.len();
                let dispatch_response = request_dispatcher_response(
                    &served_connection.intake,
                    inbox_sender,
                    |response_sender| RuntimeEvent::IpcClipboardCopyPart {
                        held_copy_byte_count,
                        response_sender,
                    },
                );
                // No dispatcher left: the connection closes with no answer.
                let Some(can_take_clipboard_tool_copy) = dispatch_response else {
                    return;
                };
                let answer_result = if can_take_clipboard_tool_copy {
                    held_copy_bytes.extend_from_slice(&copied_bytes);
                    IpcResult::ClipboardCopyPartTaken
                } else {
                    held_copy_bytes = Vec::new();
                    IpcResult::ClipboardCopyRouted {
                        should_run_clipboard_tool: true,
                    }
                };
                IpcResponse {
                    request_id,
                    answer_result,
                }
            }
            IpcRequestKind::ClipboardCopy {
                pane_id,
                clipboard_targets,
                copied_bytes,
            } => {
                // A copy that came in one part moves to the dispatcher as it is.
                let whole_copy_bytes = if held_copy_bytes.is_empty() {
                    copied_bytes
                } else {
                    held_copy_bytes.extend_from_slice(&copied_bytes);
                    std::mem::take(&mut held_copy_bytes)
                };
                let dispatch_response = request_dispatcher_response(
                    &served_connection.intake,
                    inbox_sender,
                    |response_sender| RuntimeEvent::IpcClipboardCopy {
                        pane_id,
                        clipboard_targets,
                        copied_bytes: whole_copy_bytes,
                        response_sender,
                    },
                );
                // No dispatcher left: the connection closes with no answer.
                let Some(should_run_clipboard_tool) = dispatch_response else {
                    return;
                };
                IpcResponse {
                    request_id,
                    answer_result: IpcResult::ClipboardCopyRouted {
                        should_run_clipboard_tool,
                    },
                }
            }
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
/// [`SessionEvent::MouseAnswer`] carrying that round's `request_id`. A `Ping`
/// is read and dropped: nothing crosses for it, and nothing answers it. A
/// request of any other kind, end of stream, a transport fault, an oversize
/// frame, or a dispatcher that is gone all end the reading loop. Each read of
/// the client's next bytes waits with no limit.
///
/// With a `heartbeat_interval`, the writing thread writes one
/// [`SessionEvent::Pong`] whenever it has written nothing to the connection
/// for that long. Example: a 10-second interval, a third of a 30-second link
/// timeout, sends a client whose panes stay still one `Pong` every 10
/// seconds, also while that client's own 20-second paste is still arriving.
/// A delivery that writes nothing does not put the `Pong` off: a `Pong` that
/// is due goes out before the next delivery is taken. Example: while painted
/// frames over [`MAX_FRAME_BYTE_COUNT`](transport::MAX_FRAME_BYTE_COUNT) keep
/// the queue busy, a `Pong` that falls due while the writing thread works on
/// one frame goes out before the next frame is taken.
///
/// Before each delivery it reads, the writing thread takes every byte in the
/// stream's waiting host writes and writes them as the frames
/// [`build_wire_host_write_events`] gives. A [`Delivery::HostWrite`] only
/// wakes it for that. When the writing thread ends, it drops its queue's
/// receiver, then clears the wake it did not read
/// ([`WaitingHostWrites::clear_queued_wake`]). The next host write that the
/// bus adds for this client then finds the receiver gone, and the bus takes
/// none. Example: a write that fails on a large frame, with a wake queued
/// behind that frame, ends the thread. A clipboard tool copy for this client
/// after that is not routed.
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
/// The reading loop ending detaches the client: it hands over one
/// [`RuntimeEvent::ClientDetached`], whose `is_returning` is the value of the
/// `Leaving` request that ended the loop. With no such request, it is
/// `is_returning_before_first_request` when the loop read no request, and
/// `is_remote` after the loop read one. Example: a client on this machine
/// whose Attach names its own id, after the session replaced its own process
/// image, and that ends before its first request, detaches as returning. The
/// same client ending after its first request detaches as not returning. The
/// detach removes the client's record and drops its
/// subscription; the closed queue, or the terminal `Quit` or `Restarting`
/// frame, then ends the writing thread. The writing thread hands over no
/// detach. A write that fails ends the writing thread and closes this
/// connection's read direction ([`Intake::close_read_direction`]), and the
/// reading loop then ends on a failed read. Example: on Linux, a write to a
/// client that shut its read direction fails with `EPIPE`, and that client is
/// detached while it still sends. A failed write of the goodbye or of the
/// session's last frame closes nothing. A detach for a client already gone
/// changes nothing.
///
/// A detach the server starts closes the client's queue: the writing thread
/// writes what is already queued and the waiting host-write bytes, then
/// [`SessionEvent::Detached`] as its last frame, and ends. A session ending at
/// the same moment writes only the waiting host-write bytes, and the goodbye
/// is still what this client reads. The reading half keeps reading until the
/// client closes its end, and that close reads as end of stream — a second
/// detach for a client already gone. A `Leaving` request ends the reading half
/// the same way: the client says it sends nothing more, and every key it sent
/// has already been handed over.
///
/// A reading loop that ends on a failed read — end of stream, a read direction
/// the writing thread closed, a transport fault, or an oversize frame — gets
/// no goodbye. Once the detach closes the queue, the writing thread writes
/// nothing more and ends, and the connection closes. Example: a client on
/// another machine whose router closed its connection reads the connection
/// close, not [`SessionEvent::Detached`].
///
/// `ending_notice` is what the session raises when it ends — see
/// [`EndingNotice`]. The writing thread reads it at the top of each turn. It
/// writes the waiting host-write bytes, then the frame the notice names,
/// [`SessionEvent::Quit`] or [`SessionEvent::Restarting`]. Every delivery still
/// queued goes unwritten. A client whose queue the server closed before that
/// reads [`SessionEvent::Detached`] instead, or no goodbye after a failed
/// read: it left before the session did. The thread counts itself on the
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
/// `served_connection` is this connection's entry in the [`Intake`]: the
/// intake holds this connection's read direction through it, and the writing
/// thread closes that read direction through the intake after a failed write.
/// The client's record stays as it is: the image swap that cut the connection
/// carries it across.
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
        waiting_host_writes,
        ending_notice,
        graphics_capabilities,
        heartbeat_interval,
        is_remote,
        is_returning_before_first_request,
    } = attached_stream;
    let (mut frame_reader, mut frame_writer) = connection.split();
    // Set by the reading loop when it ends on a failed read, before it hands
    // over the detach.
    let is_link_lost = Arc::new(AtomicBool::new(false));
    let writer_is_link_lost = Arc::clone(&is_link_lost);
    let writer_intake = Arc::clone(&served_connection.intake);
    let connection_number = served_connection.connection_number;
    ending_notice.record_writer_started();
    std::thread::spawn(move || {
        let mut image_cache = ConnectionImageCache::new();
        let has_write_failed = loop {
            // The session is ending. This client is told at once: the
            // host-write bytes that wait for it go out first, and everything
            // still queued goes unwritten. A queue the server already closed
            // detached this client before the session ended: its own goodbye,
            // or none after a failed read, is what it reads.
            if let Some(session_ending) = ending_notice.get_session_ending() {
                let terminal_event = loop {
                    match deliveries.try_recv() {
                        Ok(_) => {}
                        Err(mpsc::TryRecvError::Empty) => {
                            break Some(match session_ending {
                                SessionEnding::Quit => SessionEvent::Quit,
                                SessionEnding::Restarting => SessionEvent::Restarting,
                            })
                        }
                        Err(mpsc::TryRecvError::Disconnected) => {
                            break build_detach_goodbye(&writer_is_link_lost)
                        }
                    }
                };
                if let Some(terminal_event) = terminal_event {
                    if send_waiting_host_writes(&mut frame_writer, &waiting_host_writes).is_ok() {
                        let _ = frame_writer.send(&terminal_event);
                    }
                }
                break false;
            }
            if send_waiting_host_writes(&mut frame_writer, &waiting_host_writes).is_err() {
                break true;
            }
            let delivery_wait_result = match heartbeat_interval {
                // The next `Pong` is due `heartbeat_interval` after the last
                // frame that went out. A `Pong` that is due goes out before
                // the next delivery is taken.
                Some(heartbeat_interval) => {
                    let time_left_before_pong = (frame_writer.get_last_frame_written_at()
                        + heartbeat_interval)
                        .saturating_duration_since(Instant::now());
                    if time_left_before_pong.is_zero() {
                        Err(mpsc::RecvTimeoutError::Timeout)
                    } else {
                        deliveries.recv_timeout(time_left_before_pong)
                    }
                }
                None => deliveries
                    .recv()
                    .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            };
            let delivery = match delivery_wait_result {
                Ok(delivery) => delivery,
                // No frame went out for `heartbeat_interval`.
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if frame_writer.send(&SessionEvent::Pong).is_err() {
                        break true;
                    }
                    continue;
                }
                // The queue closed, which the server does when it detaches this
                // client. The wait hands back everything queued before the
                // close: the goodbye follows the events that preceded it.
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(detach_goodbye) = build_detach_goodbye(&writer_is_link_lost) {
                        if send_waiting_host_writes(&mut frame_writer, &waiting_host_writes).is_ok()
                        {
                            let _ = frame_writer.send(&detach_goodbye);
                        }
                    }
                    break false;
                }
            };
            let send_result = match &delivery {
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
                // The wake is read. The top of the next turn writes the
                // waiting bytes.
                Delivery::HostWrite => {
                    waiting_host_writes.clear_queued_wake();
                    continue;
                }
                _ => Ok(()),
            };
            if send_result.is_err() {
                break true;
            }
            if matches!(
                delivery,
                Delivery::Frame(_) | Delivery::PanePlacementSnapshot { .. }
            ) {
                continue;
            }
            let session_event = build_wire_event(&delivery);
            if let Some(session_event) = session_event {
                let has_event_write_failed = match frame_writer.send(&session_event) {
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
                if has_event_write_failed {
                    break true;
                }
                // `Quit` and `Restarting` are the stream's terminal frames; the
                // loop ends on either without waiting for the queue to close.
                if matches!(session_event, SessionEvent::Quit | SessionEvent::Restarting) {
                    break false;
                }
            }
        };
        // The queue's receiver goes before the wake clears. A host write that
        // the bus adds after this queues a new wake, finds the receiver gone,
        // and removes this client's subscriber.
        drop(deliveries);
        waiting_host_writes.clear_queued_wake();
        // A failed write closes the read direction: the reading loop then ends
        // on a failed read and hands over the detach.
        if has_write_failed {
            writer_intake.close_read_direction(connection_number);
        }
        ending_notice.record_writer_ended();
    });

    // A connection that ends with no `Leaving` request reports its client as
    // returning: `is_returning_before_first_request` until the first request
    // is read, then `is_remote`.
    let mut is_returning = is_returning_before_first_request;
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
            // An oversize frame leaves its payload unread, and a disconnect or
            // a transport fault leaves no stream. Each one ends this loop, and
            // the client gets no goodbye.
            Err(_) => {
                is_link_lost.store(true, Ordering::SeqCst);
                break;
            }
        };
        is_returning = is_remote;
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
            IpcRequestKind::Ping => continue,
            // The client sends nothing more on this connection, and everything
            // it did send is handed over above.
            IpcRequestKind::Leaving {
                is_returning: is_client_returning,
            } => {
                is_returning = is_client_returning;
                break;
            }
            IpcRequestKind::Hello { .. }
            | IpcRequestKind::Attach { .. }
            | IpcRequestKind::Discovery
            | IpcRequestKind::Layout { .. }
            | IpcRequestKind::RecentEvents
            | IpcRequestKind::Restart
            | IpcRequestKind::ClipboardCopyPart { .. }
            | IpcRequestKind::ClipboardCopy { .. } => break,
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
            is_returning,
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

/// The goodbye a writing thread writes once the server closed its client's
/// queue: [`SessionEvent::Detached`], or `None` when `is_link_lost` says the
/// reading loop ended on a failed read.
fn build_detach_goodbye(is_link_lost: &AtomicBool) -> Option<SessionEvent> {
    if is_link_lost.load(Ordering::SeqCst) {
        return None;
    }
    Some(SessionEvent::Detached)
}

/// Take every byte in `waiting_host_writes` and send them as one host write,
/// as [`send_host_write`] does. Sends nothing when no byte waits.
///
/// # Errors
/// The [`IpcError`] of the first frame that could not be written.
fn send_waiting_host_writes(
    frame_writer: &mut FrameWriter,
    waiting_host_writes: &WaitingHostWrites,
) -> Result<(), IpcError> {
    let host_write_bytes = waiting_host_writes.take_host_write_bytes();
    if host_write_bytes.is_empty() {
        return Ok(());
    }
    send_host_write(frame_writer, &host_write_bytes)
}

/// Send one painted frame and each new image record after it.
///
/// A client whose terminal has no native image protocol receives every image
/// placement through [`hide_image_records`], and no image record.
///
/// # Errors
/// The [`IpcError`] of a write that left the connection unusable, as
/// [`report_image_send_error`] sorts it.
fn send_painted_frame(
    frame_writer: &mut FrameWriter,
    image_cache: &mut ConnectionImageCache,
    render_snapshot: &RenderSnapshot,
    graphics_capabilities: GraphicsCapabilities,
    client_id: ClientId,
) -> Result<(), IpcError> {
    if !graphics_capabilities.has_native_image_protocol() {
        let mut painted_frame = build_wire_frame(render_snapshot);
        for pane_snapshot in &mut painted_frame.pane_snapshots {
            hide_image_records(&mut pane_snapshot.image_placement_snapshots);
        }
        let painted_frame_event = SessionEvent::Painted {
            frame: Box::new(painted_frame),
        };
        return match frame_writer.send(&painted_frame_event) {
            Ok(()) => Ok(()),
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

/// Send `host_write_bytes` as the frames [`build_wire_host_write_events`]
/// lists: `HostWritePart` frames of
/// [`MAX_HOST_WRITE_BYTE_COUNT`](koshi_ipc::event::MAX_HOST_WRITE_BYTE_COUNT)
/// bytes, then one `HostWrite` frame with the bytes that are left. The first
/// failed write stops the sending.
///
/// # Errors
/// The [`IpcError`] of the first frame that could not be written.
fn send_host_write(
    frame_writer: &mut FrameWriter,
    host_write_bytes: &[u8],
) -> Result<(), IpcError> {
    for host_write_event in build_wire_host_write_events(host_write_bytes) {
        frame_writer.send(&host_write_event)?;
    }
    Ok(())
}

/// Send one placement preview and each new image record after it.
///
/// A client whose terminal has no native image protocol receives every image
/// placement through [`hide_image_records`], and no image record.
///
/// # Errors
/// The [`IpcError`] of a write that left the connection unusable, as
/// [`report_image_send_error`] sorts it.
fn send_placement_snapshot(
    frame_writer: &mut FrameWriter,
    image_cache: &mut ConnectionImageCache,
    request_id: u64,
    placement_snapshot: &PlacementSnapshot,
    graphics_capabilities: GraphicsCapabilities,
    client_id: ClientId,
) -> Result<(), IpcError> {
    if !graphics_capabilities.has_native_image_protocol() {
        let mut wire_placement_snapshot = build_wire_placement_snapshot(placement_snapshot);
        hide_placement_image_records(&mut wire_placement_snapshot);
        let placement_snapshot_event = SessionEvent::PanePlacementSnapshot {
            request_id,
            snapshot: Box::new(wire_placement_snapshot),
        };
        return match frame_writer.send(&placement_snapshot_event) {
            Ok(()) => Ok(()),
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
/// A write the connection survives, a frame over the cap, stops the sending
/// and returns `Ok`.
///
/// # Errors
/// The [`IpcError`] of a write that left the connection unusable, as
/// [`report_image_send_error`] sorts it.
fn send_image_uploads(
    frame_writer: &mut FrameWriter,
    image_cache: &mut ConnectionImageCache,
    client_id: ClientId,
    leading_event: &SessionEvent,
    leading_event_part: &str,
    image_uploads: Vec<(u64, Arc<ImageRecord>)>,
) -> Result<(), IpcError> {
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
    Ok(())
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
/// `image_cache` and returns `Ok`: the connection stays usable.
///
/// # Errors
/// Every other failure, `write_error` itself: the connection is no longer
/// usable.
fn report_image_send_error(
    image_cache: &mut ConnectionImageCache,
    client_id: ClientId,
    write_error: IpcError,
    failed_image_transfer_part: &str,
) -> Result<(), IpcError> {
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
            Ok(())
        }
        write_error => {
            tracing::warn!(
                %client_id,
                failed_image_transfer_part,
                %write_error,
                "image transfer write failed"
            );
            Err(write_error)
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

/// What a check of `executable_watch` runs once the program file prints another
/// koshi version: [`restart_into_installed_version`] with that version, through
/// `connection_intake` and `inbox_sender`.
fn build_installed_version_restart(
    connection_intake: &Arc<Intake>,
    inbox_sender: &Sender<RuntimeEvent>,
    executable_watch: &Arc<ExecutableWatch>,
) -> impl FnOnce(String) + Send + 'static {
    let restart_intake = Arc::clone(connection_intake);
    let restart_inbox_sender = inbox_sender.clone();
    let restart_executable_watch = Arc::clone(executable_watch);
    move |installed_version| {
        restart_into_installed_version(
            &restart_intake,
            &restart_inbox_sender,
            &restart_executable_watch,
            &installed_version,
        );
    }
}

/// Ask the dispatcher to restart this session into its program file, which
/// now holds koshi `installed_version`, through [`RuntimeEvent::IpcRestart`],
/// and log the verdict: an accepted restart at info level, a refused one at
/// warning level with the sentence it was refused with. A refusal that
/// [`RestartRefusal::can_end_without_file_change`](crate::server::RestartRefusal::can_end_without_file_change)
/// calls [`ExecutableWatch::schedule_restart_retry`] on `executable_watch`. A closed
/// intake sends nothing.
fn restart_into_installed_version(
    connection_intake: &Intake,
    inbox_sender: &Sender<RuntimeEvent>,
    executable_watch: &ExecutableWatch,
    installed_version: &str,
) {
    match request_dispatcher_response(connection_intake, inbox_sender, |response_sender| {
        RuntimeEvent::IpcRestart { response_sender }
    }) {
        Some(Ok(())) => tracing::info!(
            installed_version,
            "the program file holds another koshi version; the session restarts into it"
        ),
        Some(Err(restart_refusal)) => {
            tracing::warn!(
                installed_version,
                %restart_refusal,
                "the program file holds another koshi version, and the session cannot restart into it"
            );
            if restart_refusal.can_end_without_file_change() {
                executable_watch.schedule_restart_retry();
            }
        }
        None => {}
    }
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
