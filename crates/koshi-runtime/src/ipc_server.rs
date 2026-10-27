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
//! A `SubmitCommand`, an `Attach`, a `ClipboardCopy` and a `Restart` change
//! the session, and a control connection runs one only after a `Prepare`
//! hands it over and a `Commit` follows. The connection holds the request in
//! between, and drops it when the connection ends first. A caller that gives
//! up before it writes the `Commit` leaves nothing behind.
//!
//! A `SubmitCommand`'s [`CommandSource`] is set here, from the connection the
//! request arrived on, over whatever source the peer wrote: a control
//! connection carries a CLI source, and an attached client's connection
//! carries [`CommandSource::KeyBinding`] naming that connection's own client.
//!
//! An `Attach` is the one request that keeps its connection: once the
//! `Attached` reply is written, the connection is split. The writing half
//! carries that client's event stream, and the reading half carries that
//! client's key presses, resizes, pasted text and commands to the dispatcher
//! and writes nothing back. The peer going away detaches the client.
//!
//! On Linux and macOS, passing [`OtherUsers`] to [`IpcServer::start`] moves the
//! control socket to the machine-wide shared directory and widens it: the
//! other local users of this machine can reach it. On Windows [`OtherUsers`]
//! has no value, and only the user who started a session reaches it. The
//! endpoint file keeps its private place and its `0600` mode either way. Every
//! accepted connection is gated by the user the OS reports for it: the user
//! who started the session is always served, and another local user is served
//! only while `allow-other-users` is on. The setting is read again for each of
//! that user's requests. Turning it off closes their connections.
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
//! [`IpcServer::start_refusing_requests`] stops every event a peer sends from
//! reaching the dispatcher. Each request that needs the dispatcher is answered
//! with [`IpcErrorCode::Restarting`], and did not run. The dispatcher calls it
//! before it stops draining the inbox, and then waits with
//! [`IpcServer::wait_until_requests_answered`] until every answer it gave is
//! written.
//!
//! [`IpcServer::shutdown`] stops accepting, joins the accept loop, and
//! removes the endpoint file and the socket.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use koshi_core::command::{CommandEnvelope, CommandSource};
use koshi_core::constant::{
    compute_heartbeat_interval, HIGHEST_MAX_FRAMES_PER_SECOND, LOWEST_MAX_FRAMES_PER_SECOND,
    MAX_VIEWPORT_CELL_COUNT, MIN_LINK_TIMEOUT_MS,
};
use koshi_core::ids::{ClientId, SessionId};
use koshi_ipc::endpoint::{
    compute_socket_address, delete_socket_file, EndpointFile, ServerProgramFile,
};
use koshi_ipc::error::IpcError;
use koshi_ipc::event::{SessionEvent, MAX_RELEASED_IMAGE_CONTENT_COUNT};
use koshi_ipc::frame::{
    FrameImagePlacement, PaintedFrame, MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT,
    MAX_FRAME_IMAGE_TRANSFER_COUNT,
};
use koshi_ipc::handshake::{Handshake, Peer};
use koshi_ipc::image_chunk::build_image_chunk_frame_head;
use koshi_ipc::plane::{self, RequestDisposition};
use koshi_ipc::protocol::{
    ConnectionToken, GraphicsCapabilities, IncomingRequest, IpcErrorCode, IpcErrorPayload,
    IpcRequestKind, IpcResponse, IpcResult, SessionPlane,
};
use koshi_ipc::transport::{self, Connection, FrameWriter, Listener, ReadCloser};
#[cfg(unix)]
use koshi_ipc::validate::validate_shared_socket_address;
use koshi_ipc::validate::{reclaim_stale_socket, validate_socket_address};
use koshi_ipc::wire::MaybeKnown;
use koshi_observability::logging::recent_events;
use koshi_observability::repeated_warning::WarningRateLimiter;
use koshi_renderer::snapshot::{Delivery, ImagePlacementSnapshot, RenderSnapshot};
use koshi_terminal::graphics::{DecodedAnimation, DecodedImage, ImageRecord};

use crate::executable_watch::ExecutableWatch;
use crate::runtime::bus::{build_wire_event, build_wire_host_write_events, WriterHandoff};
use crate::runtime::delivery_queue::{DeliveryReceiver, PacedReceive, PaintedUpdateReport};
use crate::runtime::event::{EndingNotice, RunningCount, RuntimeEvent, SessionEnding};
use crate::runtime::frame::{
    build_wire_frame, build_wire_frame_with_content_ids, build_wire_image_chunk_sources,
    build_wire_image_transfer,
};
use crate::runtime::painted_update::PaintedUpdateSender;
use crate::server::{RestartCheck, RestartRefusal};

mod paint_window;

use paint_window::PaintWindow;

/// How long the accept loop sleeps after a failed accept before it accepts
/// again. A persistent accept error — say, the process is out of file
/// descriptors — retries once per interval.
const ACCEPT_RETRY_DELAY_DURATION: Duration = Duration::from_millis(100);

/// The version of the binary this session server is, reported in its Hello
/// answer.
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The sentence that refuses a clipboard copy, or a part of one, on a
/// connection from another machine or from another user of this machine.
const CLIPBOARD_COPY_ORIGIN_REFUSAL: &str =
    "a pane clipboard copy must come from this machine and this user";

/// The sentence that refuses, with [`IpcErrorCode::Restarting`], a request
/// read while the session replaces its process image.
const SESSION_RESTARTING_MESSAGE: &str = "the session is restarting; ask again in a moment";

/// Reads the `allow-other-users` setting from the configuration again and
/// reports whether it is on. Called for each connection from another local
/// user and for each request that user sends.
pub type OtherUsersSetting = Arc<dyn Fn() -> bool + Send + Sync>;

/// What [`IpcServer::start`] needs to serve the other local users of this
/// machine.
///
/// Only the control socket moves: it binds in this user's directory under the
/// machine-wide shared directory, at mode `0666`. The endpoint file stays in
/// the private runtime directory at mode `0600`: the token it carries stays
/// readable only by the user who started the session.
#[cfg(unix)]
pub struct OtherUsers {
    /// The machine-wide directory koshi shares between local users, from
    /// `koshi_paths::resolve_shared_sessions_directory`. This user's directory inside it
    /// holds the control socket.
    pub shared_directory: PathBuf,
    /// The live read of the `allow-other-users` setting.
    pub is_enabled: OtherUsersSetting,
}

/// On Windows no value exists, and [`IpcServer::start`] always takes `None`:
/// only the user who started a session reaches it.
#[cfg(windows)]
pub enum OtherUsers {}

/// What the control socket takes in: every connection being served, how many of
/// them carry an attached client, how many requests wait for their answer to be
/// written, and whether what a peer sends still reaches the session.
///
/// A serving thread reads `is_refusing_requests` and hands its event to the
/// dispatcher under one shared borrow of the intake, and
/// [`start_refusing_requests`](Intake::start_refusing_requests) sets it under
/// the exclusive borrow. A hand-off either finishes before that call, or reads
/// the refusing intake and never happens. Once that call returns, no serving
/// thread hands over another event.
#[derive(Debug, Default)]
struct Intake {
    intake_state: RwLock<IntakeState>,
    /// How many requests were read and wait for their answer to be written,
    /// each request held by a [`Prepare`](IpcRequestKind::Prepare) answered
    /// `Prepared` included until its `Commit` is answered.
    unanswered_requests: RunningCount,
}

/// What an [`Intake`] holds.
#[derive(Debug, Default)]
struct IntakeState {
    /// `true` from [`Intake::start_refusing_requests`] until
    /// [`Intake::stop_refusing_requests`]. No event reaches the dispatcher
    /// while it is set, and every request that needs the dispatcher is refused
    /// with [`IpcErrorCode::Restarting`].
    is_refusing_requests: bool,
    /// How many connections carry an attached client and are still being read.
    attached_connection_count: usize,
    /// The read direction of every connection being served, under the number
    /// the intake filed it as. The [`ServedConnection`] its thread holds
    /// removes the entry when that thread ends.
    reader_by_connection_number: HashMap<u64, ReadCloser>,
    /// The number the next accepted connection is filed under.
    next_connection_number: u64,
}

/// Why an event handed to an [`Intake`] did not reach the dispatcher, or why
/// no answer to it came back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchFailure {
    /// The intake refuses requests: the event was dropped.
    Refused,
    /// The dispatcher is gone: the event was dropped, or no answer came.
    DispatcherGone,
}

impl Intake {
    /// File `connection`'s read direction and hand back the entry its serving
    /// thread holds. `None` means the connection is not served: its read
    /// direction could not be taken.
    fn accept_connection(self: &Arc<Self>, connection: &Connection) -> Option<ServedConnection> {
        let reader = connection.create_read_closer().ok()?;
        let mut intake_state = self.intake_state.write().expect("intake");
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

    /// Hand `runtime_event` to the dispatcher over the runtime inbox.
    ///
    /// # Errors
    /// [`DispatchFailure::Refused`] while the intake refuses requests, and
    /// [`DispatchFailure::DispatcherGone`] once the dispatcher is gone. The
    /// event is dropped in both cases.
    fn hand_over_event(
        &self,
        inbox_sender: &Sender<RuntimeEvent>,
        runtime_event: RuntimeEvent,
    ) -> Result<(), DispatchFailure> {
        let intake_state = self.intake_state.read().expect("intake");
        if intake_state.is_refusing_requests {
            return Err(DispatchFailure::Refused);
        }
        inbox_sender
            .send(runtime_event)
            .map_err(|_| DispatchFailure::DispatcherGone)
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

    /// Count one request as waiting for its answer to be written, and hand
    /// back the entry the serving thread holds until that answer is written or
    /// the connection ends.
    fn record_unanswered_request(self: &Arc<Self>) -> UnansweredRequest {
        self.unanswered_requests.record_started();
        UnansweredRequest {
            intake: Arc::clone(self),
        }
    }

    /// Wait until no request waits for its answer to be written, or until
    /// `answer_deadline` passes. Gives how many requests still wait.
    fn wait_until_requests_answered(&self, answer_deadline: Instant) -> usize {
        self.unanswered_requests
            .wait_until_none_run(answer_deadline)
    }

    /// Refuse every event from here on: no serving thread hands another event
    /// to the dispatcher, and every request that needs the dispatcher is
    /// answered with [`IpcErrorCode::Restarting`]. Connections are still
    /// accepted and served.
    fn start_refusing_requests(&self) {
        self.intake_state
            .write()
            .expect("intake")
            .is_refusing_requests = true;
    }

    /// Hand events to the dispatcher again after
    /// [`start_refusing_requests`](Intake::start_refusing_requests).
    fn stop_refusing_requests(&self) {
        self.intake_state
            .write()
            .expect("intake")
            .is_refusing_requests = false;
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

/// One request counted in its [`Intake`] as waiting for its answer to be
/// written, while the serving thread holds this. Dropping it counts the
/// request as answered and wakes
/// [`Intake::wait_until_requests_answered`].
#[derive(Debug)]
struct UnansweredRequest {
    /// The intake this request is counted in.
    intake: Arc<Intake>,
}

impl Drop for UnansweredRequest {
    fn drop(&mut self) {
        self.intake.unanswered_requests.record_ended();
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
    deliveries: DeliveryReceiver,
    /// The host-write bytes that wait for the client's own terminal, which the
    /// writer takes before each delivery and before its last frame, and the
    /// owed-frame mark, which the writer takes after each delivery.
    writer_handoff: Arc<WriterHandoff>,
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

/// What a session that can replace its own process image hands its control
/// socket: the watch of the program file it started from, and the check every
/// restart request runs before it reaches the dispatcher.
pub struct ImageReplacement {
    /// The watch of the program file the session started from.
    pub executable_watch: Arc<ExecutableWatch>,
    /// What a restart must promise. It runs on the thread that serves the
    /// `Restart` request, or on the thread that read the program file's
    /// version.
    pub restart_check: RestartCheck,
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
    /// the user who started the session can reach it. `Some`, on Linux and
    /// macOS, binds it in this user's directory under the machine-wide shared
    /// directory instead and opens it to the other local users at mode `0666`.
    /// The endpoint file is written to the same private path at the same `0600`
    /// mode either way; only the address it carries differs.
    ///
    /// `image_replacement` `Some` lets the session replace its own process
    /// image. Its watch checks the program file the session started from on
    /// each connection the socket takes, as
    /// [`ExecutableWatch::check_executable_file`] states. A file that now
    /// holds another koshi version runs the restart check and then sends the
    /// dispatcher [`RuntimeEvent::IpcRestart`], as a `Restart` request does.
    /// The session also writes its [`ServerProgramFile`] after the endpoint
    /// file, from the watch's path and running version, and a failed write
    /// unwinds the bind as a failed endpoint write does. `None` writes no
    /// program file, and every `Restart` is refused with
    /// [`RestartRefusal::ImageReplacementUnsupported`].
    pub fn start(
        runtime_directory: &Path,
        session_id: SessionId,
        inbox_sender: Sender<RuntimeEvent>,
        other_users: Option<OtherUsers>,
        image_replacement: Option<ImageReplacement>,
    ) -> Result<IpcServer, IpcError> {
        koshi_paths::ensure_private_directory(runtime_directory).map_err(|directory_error| {
            IpcError::Transport {
                error_detail: format!(
                    "could not create the runtime directory {}: {directory_error}",
                    runtime_directory.display()
                ),
            }
        })?;
        let (socket_address, allow_other_users_setting) = match other_users {
            None => {
                let socket_address = compute_socket_address(runtime_directory, session_id);
                validate_socket_address(&socket_address, runtime_directory)?;
                (socket_address, None)
            }
            #[cfg(unix)]
            Some(other_users) => {
                let shared_user_directory =
                    ensure_shared_directories(&other_users.shared_directory)?;
                let socket_address = compute_socket_address(&shared_user_directory, session_id);
                validate_shared_socket_address(&socket_address, &shared_user_directory)?;
                (socket_address, Some(other_users.is_enabled))
            }
        };
        reclaim_stale_socket(&socket_address)?;
        let listener = Listener::bind(&socket_address)?;
        #[cfg(unix)]
        if allow_other_users_setting.is_some() {
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
        let image_replacement = image_replacement.map(Arc::new);
        let program_file_path = image_replacement.as_ref().map(|_| {
            ServerProgramFile::resolve_session_program_file_path(runtime_directory, session_id)
        });
        let advertisement_result =
            endpoint_file.write_to_path(&endpoint_path).and_then(|()| {
                match (&image_replacement, &program_file_path) {
                    (Some(image_replacement), Some(program_file_path)) => image_replacement
                        .executable_watch
                        .build_server_program_file()
                        .write_to_path(program_file_path),
                    _ => Ok(()),
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
                image_replacement.as_ref(),
            );
        });

        Ok(IpcServer {
            socket_address,
            endpoint_path,
            program_file_path,
            is_shutting_down,
            connection_token,
            connection_intake,
            accept_thread: Some(accept_thread),
        })
    }

    /// Refuse every request from here on: no event a peer sends is handed to
    /// the dispatcher, and every request that needs the dispatcher is answered
    /// with [`IpcErrorCode::Restarting`]. Connections are still accepted and
    /// served, and a Hello is still answered.
    ///
    /// A request a peer sent before this either reached the dispatcher already
    /// or is refused. The dispatcher's next pass over the runtime inbox is the
    /// last that can find anything a peer sent, and the serving thread writes
    /// each answer that pass gives.
    ///
    /// A request held by a `Prepare` answered `Prepared` is refused at its
    /// `Commit`, and does not run. Refusing again changes nothing.
    pub fn start_refusing_requests(&self) {
        self.connection_intake.start_refusing_requests();
    }

    /// Wait until every request read on a connection has its answer written,
    /// or until `answer_deadline` passes, and give how many requests still
    /// wait. A request held by a `Prepare` answered `Prepared` waits until its
    /// `Commit` is answered or its connection ends.
    ///
    /// Example: after [`start_refusing_requests`](Self::start_refusing_requests),
    /// a `Commit` that arrives 5 ms after that call is refused, its refusal is
    /// written, and the wait returns `0`.
    #[must_use]
    pub fn wait_until_requests_answered(&self, answer_deadline: Instant) -> usize {
        self.connection_intake
            .wait_until_requests_answered(answer_deadline)
    }

    /// Mint a fresh connection token, accept it from the next connection on,
    /// and rewrite the endpoint file with it.
    ///
    /// The address does not change. A connection already open keeps serving:
    /// the token is checked at Hello only. After
    /// [`start_refusing_requests`](Self::start_refusing_requests), each request
    /// read from here on reaches the dispatcher again.
    ///
    /// # Errors
    /// Returns the failure of writing the endpoint file. The fresh token is
    /// the one this server accepts either way: the endpoint file then
    /// advertises a token no connection is served under.
    pub fn rotate_token(&self) -> Result<(), IpcError> {
        let fresh_connection_token = ConnectionToken::generate();
        // The fresh token is stored before requests reach the dispatcher again
        // and before the endpoint file advertises it.
        *self.connection_token.write().expect("connection token") = fresh_connection_token.clone();
        self.connection_intake.stop_refusing_requests();
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
    /// joined, and the program file, the endpoint file and the socket are
    /// removed, in that order. Connections already being
    /// served run out on their own threads; with the dispatcher draining, their
    /// in-flight requests end in a closed connection and change nothing.
    ///
    /// Dropping an `IpcServer` runs the same teardown, including when a panic
    /// unwinds the server.
    pub fn shutdown(self) {
        drop(self);
    }

    /// The teardown. A second call joins no thread, since the first took
    /// `accept_thread`, and removing a file that is already gone does nothing.
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
        delete_socket_file(&self.socket_address);
    }
}

/// Create the machine-wide shared directory and this user's directory inside
/// it, and hand back this user's directory: where a session other local users
/// may reach binds its control socket.
#[cfg(unix)]
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
/// this machine may connect to it.
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
/// serving thread. A failed accept is logged at warn level at most once inside
/// [`LOG_WINDOW_DURATION`](koshi_observability::repeated_warning::LOG_WINDOW_DURATION),
/// pauses for [`ACCEPT_RETRY_DELAY_DURATION`] and retries.
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
/// whose read direction could not be taken is closed without being served.
///
/// Each connection the intake takes runs a check of the watch
/// `image_replacement` holds, and its serving thread runs
/// [`ExecutableWatch::check_executable_file_after_refused_hello`] for each
/// Hello refused for its protocol version. When either finds another koshi
/// version, [`restart_into_installed_version`] runs the restart check and asks
/// the dispatcher to restart.
fn run_accept_loop(
    listener: &Listener,
    connection_token: &RwLock<ConnectionToken>,
    inbox_sender: &Sender<RuntimeEvent>,
    is_shutting_down: &AtomicBool,
    allow_other_users_setting: Option<&OtherUsersSetting>,
    connection_intake: &Arc<Intake>,
    image_replacement: Option<&Arc<ImageReplacement>>,
) {
    let mut accept_failure_warning = WarningRateLimiter::new();
    transport::accept_until_shutdown(
        listener,
        is_shutting_down,
        ACCEPT_RETRY_DELAY_DURATION,
        |accept_error| {
            if accept_failure_warning.record_warning_if_due(Instant::now()) {
                tracing::warn!(
                    %accept_error,
                    "the session could not accept a connection; \
                     retrying every {ACCEPT_RETRY_DELAY_DURATION:?}"
                );
            }
        },
        |connection| {
            // The OS reports which user opened the connection.
            let Ok(is_same_user) = connection.is_peer_same_user() else {
                return;
            };
            let Some(served_connection) = connection_intake.accept_connection(&connection) else {
                return;
            };
            if let Some(image_replacement) = image_replacement {
                image_replacement.executable_watch.check_executable_file(
                    build_installed_version_restart(
                        connection_intake,
                        inbox_sender,
                        image_replacement,
                    ),
                );
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
            let image_replacement = image_replacement.cloned();
            std::thread::spawn(move || {
                serve_connection(
                    connection,
                    connection_token,
                    &inbox_sender,
                    peer,
                    live_setting,
                    &served_connection,
                    image_replacement.as_ref(),
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
/// A `SubmitCommand`, an `Attach`, a `ClipboardCopy` and a `Restart` run only
/// through `Prepare` and `Commit`:
///
/// 1. A `Prepare` carrying one of them asks the dispatcher for an answer
///    first: [`RuntimeEvent::IpcClipboardCopyPrepare`] for a `ClipboardCopy`,
///    and [`RuntimeEvent::IpcPrepare`] for the others. The connection then
///    holds the request and answers `Prepared`. A `ClipboardCopy` that the
///    dispatcher says no client would take is answered `ClipboardCopyRouted`
///    with `should_run_clipboard_tool` set to `true`, the connection drops
///    every byte it holds, and nothing is held.
/// 2. A `Commit` runs the held request as this function runs that kind below,
///    and its answer goes out under the `Commit`'s request id.
/// 3. A `Ping` leaves the held request in place. Every other request this
///    function serves drops it, and the clipboard bytes the connection holds,
///    before it is served. A Hello, a frame that does not decode and a kind
///    this build does not have are answered by [`plane::read_next_request`]
///    and leave it in place.
///
/// Each refusal below is [`IpcErrorCode::RequestFailed`], and the connection
/// keeps serving:
///
/// - one of the four kinds that arrives without `Prepare`, with the sentence
///   `the session runs Restart requests only after Prepare and Commit` for a
///   `Restart`;
/// - a `Prepare` carrying any other kind, with `the session cannot prepare
///   Discovery requests` for a `Discovery`;
/// - a `Commit` with nothing held, with `no request waits for a Commit on this
///   connection`.
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
/// 2. A committed `ClipboardCopy` hands the dispatcher the held bytes, then
///    its own bytes, as one copy
///    ([`Server::route_clipboard_tool_copy`](crate::server::Server::route_clipboard_tool_copy)),
///    and answers `ClipboardCopyRouted` with the dispatcher's answer. The
///    connection then holds no bytes.
///
/// A `ClipboardCopyPart`, a `Prepare` carrying a `ClipboardCopy`, and a
/// committed `ClipboardCopy` are refused with [`IpcErrorCode::RequestFailed`]
/// on a connection from another machine or from another local user. The
/// connection drops every byte it holds, and keeps serving.
///
/// A `SubmitCommand` on this connection has its source stamped by
/// [`stamp_cli_command_source`] before it crosses to the dispatcher: this connection carries
/// a `koshi` CLI invocation.
///
/// A `Restart` runs the restart check `image_replacement` holds on this
/// thread before anything reaches the dispatcher. A check that refuses, and a
/// connection served with no `image_replacement`, are answered with
/// [`IpcErrorCode::RequestFailed`] carrying the sentence naming what is wrong,
/// and the connection keeps serving. A check that passes sends
/// [`RuntimeEvent::IpcRestart`], and the answer is `Restarting`.
///
/// An answered `Attach` ends the request loop: the connection is handed to
/// [`stream_events`], which carries that client's events out and that client's
/// input in for as long as the connection lives. An `Attached` answer that
/// cannot be written detaches the client: as returning when the connection
/// came from another machine or the `Attach` names a client to come back as
/// (`resume_client_id`), and as not returning otherwise. An `Attach` whose
/// `viewport_size` holds more than [`MAX_VIEWPORT_CELL_COUNT`] cells reaches
/// no dispatcher: it is refused with [`IpcErrorCode::RequestFailed`] and the
/// sentence `a viewport of 65535x65535 cells is over the limit of 2097152
/// cells` for `65535x65535`, and the connection keeps serving. An `Attach`
/// whose `link_timeout_ms` is below [`MIN_LINK_TIMEOUT_MS`], `1000`, reaches no
/// dispatcher: it is refused with [`IpcErrorCode::RequestFailed`] and the
/// sentence `a link timeout of 999 ms is below the minimum of 1000 ms` for
/// `999`, and the connection keeps serving. An `Attach` whose
/// `max_frames_per_second` is outside [`LOWEST_MAX_FRAMES_PER_SECOND`] to
/// [`HIGHEST_MAX_FRAMES_PER_SECOND`], `30` to `120`, reaches no dispatcher
/// either: it is refused with [`IpcErrorCode::RequestFailed`] and the sentence
/// `a frame rate of 0 frames a second is outside 30 to 120` for `0`, and the
/// connection keeps serving.
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
/// when this function returns. Each request read is counted in the intake as
/// unanswered until its answer is written, the `Attached` answer of an attach
/// included, or until the connection ends. A request held by a `Prepare`
/// answered `Prepared` stays counted until its `Commit` is answered. While the
/// intake refuses requests, a request that needs the dispatcher is answered
/// with [`IpcErrorCode::Restarting`] and [`SESSION_RESTARTING_MESSAGE`], and
/// the connection ends.
///
/// A Hello refused for its protocol version runs
/// [`ExecutableWatch::check_executable_file_after_refused_hello`] on the watch
/// `image_replacement` holds, which restarts the session through
/// [`restart_into_installed_version`] when the program file holds another
/// koshi version, and the connection keeps serving.
fn serve_connection(
    mut connection: Connection,
    connection_token: ConnectionToken,
    inbox_sender: &Sender<RuntimeEvent>,
    peer: Peer,
    live_setting: Option<OtherUsersSetting>,
    served_connection: &ServedConnection,
    image_replacement: Option<&Arc<ImageReplacement>>,
) {
    let mut handshake = Handshake::from_expected_token_and_peer(connection_token, peer);
    let mut held_copy_bytes: Vec<u8> = Vec::new();
    let mut prepared_request_kind: Option<IpcRequestKind> = None;
    // The request a `Prepare` answered `Prepared` holds stays counted as
    // unanswered until its `Commit` is answered.
    let mut prepared_unanswered_request: Option<UnansweredRequest> = None;
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
                if let Some(image_replacement) = image_replacement {
                    image_replacement
                        .executable_watch
                        .check_executable_file_after_refused_hello(
                            build_installed_version_restart(
                                &served_connection.intake,
                                inbox_sender,
                                image_replacement,
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
        let (request_kind, is_committed) = match request_kind {
            IpcRequestKind::Commit => match prepared_request_kind.take() {
                Some(committed_request_kind) => (committed_request_kind, true),
                None => (IpcRequestKind::Commit, false),
            },
            IpcRequestKind::Ping if prepared_request_kind.is_some() => continue,
            request_kind => {
                if prepared_request_kind.take().is_some() {
                    held_copy_bytes = Vec::new();
                }
                (request_kind, false)
            }
        };
        let unanswered_request = match prepared_unanswered_request.take() {
            Some(prepared_unanswered_request) => prepared_unanswered_request,
            None => served_connection.intake.record_unanswered_request(),
        };

        let outgoing_response = match request_kind {
            IpcRequestKind::Hello { .. } => {
                unreachable!("Hello is answered by the connection thread before dispatch")
            }
            IpcRequestKind::Commit => build_request_failed_response(
                request_id,
                "no request waits for a Commit on this connection".to_owned(),
            ),
            uncommitted_request_kind @ (IpcRequestKind::SubmitCommand { .. }
            | IpcRequestKind::Attach { .. }
            | IpcRequestKind::ClipboardCopy { .. }
            | IpcRequestKind::Restart)
                if !is_committed =>
            {
                build_request_failed_response(
                    request_id,
                    format!(
                        "the session runs {} requests only after Prepare and Commit",
                        uncommitted_request_kind.get_request_kind_name()
                    ),
                )
            }
            IpcRequestKind::Prepare(prepared_kind) => match *prepared_kind {
                IpcRequestKind::ClipboardCopy { .. }
                    if handshake.is_remote_caller() || live_setting.is_some() =>
                {
                    held_copy_bytes = Vec::new();
                    build_request_failed_response(
                        request_id,
                        CLIPBOARD_COPY_ORIGIN_REFUSAL.to_owned(),
                    )
                }
                IpcRequestKind::ClipboardCopy {
                    pane_id,
                    clipboard_targets,
                    copied_bytes,
                } => {
                    let copied_byte_count = held_copy_bytes.len() + copied_bytes.len();
                    let dispatch_response = request_dispatcher_response(
                        &served_connection.intake,
                        inbox_sender,
                        &mut connection,
                        request_id,
                        |response_sender| RuntimeEvent::IpcClipboardCopyPrepare {
                            pane_id,
                            copied_byte_count,
                            response_sender,
                        },
                    );
                    // No dispatcher left, or a refusal while the session
                    // restarts: the connection ends.
                    let Some(can_route_clipboard_tool_copy) = dispatch_response else {
                        return;
                    };
                    let answer_result = if can_route_clipboard_tool_copy {
                        prepared_request_kind = Some(IpcRequestKind::ClipboardCopy {
                            pane_id,
                            clipboard_targets,
                            copied_bytes,
                        });
                        IpcResult::Prepared
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
                preparable_request_kind @ (IpcRequestKind::SubmitCommand { .. }
                | IpcRequestKind::Attach { .. }
                | IpcRequestKind::Restart) => {
                    let dispatch_response = request_dispatcher_response(
                        &served_connection.intake,
                        inbox_sender,
                        &mut connection,
                        request_id,
                        |response_sender| RuntimeEvent::IpcPrepare { response_sender },
                    );
                    // No dispatcher left, or a refusal while the session
                    // restarts: the connection ends.
                    if dispatch_response.is_none() {
                        return;
                    }
                    prepared_request_kind = Some(preparable_request_kind);
                    IpcResponse {
                        request_id,
                        answer_result: IpcResult::Prepared,
                    }
                }
                unpreparable_request_kind => build_request_failed_response(
                    request_id,
                    format!(
                        "the session cannot prepare {} requests",
                        unpreparable_request_kind.get_request_kind_name()
                    ),
                ),
            },
            IpcRequestKind::SubmitCommand {
                command_envelope, ..
            } => {
                let stamped_command_envelope =
                    Box::new(stamp_cli_command_source(*command_envelope));
                let dispatch_response = request_dispatcher_response(
                    &served_connection.intake,
                    inbox_sender,
                    &mut connection,
                    request_id,
                    |response_sender| RuntimeEvent::Ipc {
                        command_envelope: stamped_command_envelope,
                        fired_action: None,
                        response_sender,
                    },
                );
                // No dispatcher left, or a refusal while the session restarts:
                // the connection ends.
                let Some(command_result) = dispatch_response else {
                    return;
                };
                IpcResponse {
                    request_id,
                    answer_result: IpcResult::CommandResult(command_result),
                }
            }
            IpcRequestKind::Attach { viewport_size, .. }
                if viewport_size.compute_cell_count() > MAX_VIEWPORT_CELL_COUNT =>
            {
                build_request_failed_response(
                    request_id,
                    format!(
                        "a viewport of {}x{} cells is over the limit of \
                         {MAX_VIEWPORT_CELL_COUNT} cells",
                        viewport_size.column_count, viewport_size.row_count
                    ),
                )
            }
            IpcRequestKind::Attach {
                link_timeout_ms: Some(link_timeout_ms),
                ..
            } if link_timeout_ms < MIN_LINK_TIMEOUT_MS => build_request_failed_response(
                request_id,
                format!(
                    "a link timeout of {link_timeout_ms} ms is below the minimum of \
                     {MIN_LINK_TIMEOUT_MS} ms"
                ),
            ),
            IpcRequestKind::Attach {
                max_frames_per_second,
                ..
            } if !(LOWEST_MAX_FRAMES_PER_SECOND..=HIGHEST_MAX_FRAMES_PER_SECOND)
                .contains(&max_frames_per_second) =>
            {
                build_request_failed_response(
                    request_id,
                    format!(
                        "a frame rate of {max_frames_per_second} frames a second is outside \
                         {LOWEST_MAX_FRAMES_PER_SECOND} to {HIGHEST_MAX_FRAMES_PER_SECOND}"
                    ),
                )
            }
            IpcRequestKind::Attach {
                viewport_size,
                resume_client_id,
                resume_token,
                pane_area,
                graphics_capabilities,
                cell_size,
                link_timeout_ms,
                max_frames_per_second,
                placement_preview_target,
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
                    &mut connection,
                    request_id,
                    |response_sender| RuntimeEvent::IpcAttach {
                        resume_client_id,
                        resume_token,
                        viewport_size,
                        pane_area,
                        cell_size,
                        attached_at: SystemTime::now(),
                        is_remote: handshake.is_remote_caller(),
                        max_frames_per_second,
                        placement_preview_target,
                        response_sender,
                    },
                );
                // No running session, no dispatcher left to mint the client,
                // or a refusal while the session restarts: the connection
                // ends.
                let Some(Some(accepted_client)) = dispatch_response else {
                    return;
                };
                // Counted before the answer is written and dropped once the
                // stream below stops reading, before its detach is handed
                // over. A caller that reads its `Attached` frame finds this
                // connection counted.
                let attached_connection = served_connection.intake.record_attached_connection();
                let attached_response = IpcResponse {
                    request_id,
                    answer_result: IpcResult::Attached {
                        client_id: accepted_client.client_id,
                        session_id: accepted_client.session_id,
                        notification_queue: accepted_client.notification_queue,
                        resume_token: Some(accepted_client.resume_token),
                        pane_area: accepted_client.pane_area,
                    },
                };
                if connection.send(&attached_response).is_err() {
                    drop(attached_connection);
                    let _ = served_connection.intake.hand_over_event(
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
                drop(unanswered_request);
                // The reply is written; from here the connection carries
                // the client's event stream and the client's own input.
                stream_events(
                    connection,
                    AttachedStream {
                        client_id: accepted_client.client_id,
                        deliveries: accepted_client.deliveries,
                        writer_handoff: accepted_client.writer_handoff,
                        ending_notice: accepted_client.ending_notice,
                        graphics_capabilities,
                        heartbeat_interval: link_timeout_ms.map(compute_heartbeat_interval),
                        is_remote: handshake.is_remote_caller(),
                        is_returning_before_first_request,
                    },
                    attached_connection,
                    inbox_sender,
                    live_setting,
                    served_connection,
                );
                return;
            }
            // A keyboard event, a resize, a cell size, a paste, a mouse round,
            // a placement preview target, a placement confirm, a ping and a
            // paint report belong on an attached client's connection, which
            // the `Attach` arm above hands to `stream_events`. On this control
            // path they name no client and close the connection.
            IpcRequestKind::Keyboard { .. }
            | IpcRequestKind::Resize { .. }
            | IpcRequestKind::CellSize { .. }
            | IpcRequestKind::Paste { .. }
            | IpcRequestKind::Mouse(_)
            | IpcRequestKind::SetPanePlacementPreview { .. }
            | IpcRequestKind::ConfirmPanePlacement { .. }
            | IpcRequestKind::Ping
            | IpcRequestKind::PaintedUpdate { .. } => return,
            IpcRequestKind::Discovery => {
                let dispatch_response = request_dispatcher_response(
                    &served_connection.intake,
                    inbox_sender,
                    &mut connection,
                    request_id,
                    |response_sender| RuntimeEvent::IpcDiscovery { response_sender },
                );
                // No running session, no dispatcher left, or a refusal while
                // the session restarts: the connection ends.
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
                    &mut connection,
                    request_id,
                    |response_sender| RuntimeEvent::IpcLayout {
                        tab_id,
                        response_sender,
                    },
                );
                // No running session, no dispatcher left, or a refusal while
                // the session restarts: the connection ends.
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
                let restart_verdict = match image_replacement {
                    Some(image_replacement) => (image_replacement.restart_check)(),
                    None => Err(RestartRefusal::ImageReplacementUnsupported),
                };
                if let Err(restart_refusal) = restart_verdict {
                    // Nothing reached the dispatcher, and the connection
                    // keeps serving.
                    build_request_failed_response(request_id, restart_refusal.to_string())
                } else {
                    let dispatch_response = request_dispatcher_response(
                        &served_connection.intake,
                        inbox_sender,
                        &mut connection,
                        request_id,
                        |response_sender| RuntimeEvent::IpcRestart { response_sender },
                    );
                    // No dispatcher left, or a refusal while the session
                    // restarts: the connection ends.
                    if dispatch_response.is_none() {
                        return;
                    }
                    IpcResponse {
                        request_id,
                        answer_result: IpcResult::Restarting,
                    }
                }
            }
            // No answer belongs to this one.
            IpcRequestKind::Leaving { .. } => return,
            IpcRequestKind::ClipboardCopyPart { .. } | IpcRequestKind::ClipboardCopy { .. }
                if handshake.is_remote_caller() || live_setting.is_some() =>
            {
                held_copy_bytes = Vec::new();
                build_request_failed_response(request_id, CLIPBOARD_COPY_ORIGIN_REFUSAL.to_owned())
            }
            IpcRequestKind::ClipboardCopyPart { copied_bytes } => {
                let held_copy_byte_count = held_copy_bytes.len() + copied_bytes.len();
                let dispatch_response = request_dispatcher_response(
                    &served_connection.intake,
                    inbox_sender,
                    &mut connection,
                    request_id,
                    |response_sender| RuntimeEvent::IpcClipboardCopyPart {
                        held_copy_byte_count,
                        response_sender,
                    },
                );
                // No dispatcher left, or a refusal while the session restarts:
                // the connection ends.
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
                    &mut connection,
                    request_id,
                    |response_sender| RuntimeEvent::IpcClipboardCopy {
                        pane_id,
                        clipboard_targets,
                        copied_bytes: whole_copy_bytes,
                        response_sender,
                    },
                );
                // No dispatcher left, or a refusal while the session restarts:
                // the connection ends.
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
        if prepared_request_kind.is_some() {
            prepared_unanswered_request = Some(unanswered_request);
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
/// them: keyboard, resize, and paste requests are reflected by the next painted
/// frame; a submitted command is reflected by the next painted frame and, for
/// a command from the client's key binding that the session rejected, one
/// [`SessionEvent::CommandRejected`] carrying its `command_id`; and a `Mouse`
/// round is answered on the writing half by exactly one
/// [`SessionEvent::MouseAnswer`] carrying that round's `request_id`, after one
/// [`SessionEvent::CommandRejected`] for each floating pane move in the round
/// that the session rejected. A client whose queue is full reads those
/// rejections in the next [`SessionEvent::Resync`] instead. A `Resize` whose
/// viewport holds more
/// than [`MAX_VIEWPORT_CELL_COUNT`] cells is read and dropped, and the client
/// keeps the viewport it had. A `Ping` is read and dropped: nothing crosses for
/// it, and nothing answers it. A `PaintedUpdate` reaches the writing thread as
/// the client's paint report
/// ([`PaintReporter`](crate::runtime::delivery_queue::PaintReporter)), and
/// nothing answers it. A request of any other kind, end of stream, a transport
/// fault, an oversize frame, or a dispatcher that is gone all end the reading
/// loop. Each read of the client's next bytes waits with no limit.
///
/// The writing thread paces painted frames by what the client painted. A frame
/// or a resync snapshot at the head of the queue goes out only while the
/// connection's paint window lets an update go out: no update waits for a
/// paint report, or the client has reported a paint and the bytes written
/// after the last update it reported are below the window. Every delivery
/// behind that frame waits with it, and a newer frame takes the place of a
/// frame at the tail of the queue. Example: a client that paints 60 times a second while
/// a pane floods it reads the newest screen after each paint, not a queue of
/// older ones. A report of an update this connection did not send closes the
/// read direction, as a failed write does. Once the server closes the queue,
/// the frames that wait go out regardless of the window.
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
/// ([`WriterHandoff::clear_queued_wake`]). The next host write that the
/// bus adds for this client then finds the receiver gone, and the bus takes
/// none. Example: a write that fails on a large frame, with a wake queued
/// behind that frame, ends the thread. A clipboard tool copy for this client
/// after that is not routed.
///
/// After each delivery it reads, the writing thread takes the stream's
/// owed-frame mark ([`WriterHandoff::take_owed_frame_mark`]). A mark that was
/// set hands one [`RuntimeEvent::QueueRoomFreed`] naming `client_id` to the
/// dispatcher unless the intake refuses requests
/// ([`Intake::hand_over_event`]). Example: a
/// frame that found the queue full on an idle session is followed by a newer
/// frame once the writing thread reads the next delivery.
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
/// `attached_connection` is this connection's place in the intake's attached
/// count, taken before the `Attached` frame is written. It is dropped once the
/// reading loop ends, before the detach is handed over: a dispatcher woken by
/// that detach reads a count that no longer holds this connection.
///
/// `served_connection` is this connection's entry in the [`Intake`]: the
/// intake holds this connection's read direction through it, and the writing
/// thread closes that read direction through the intake after a failed write.
fn stream_events(
    connection: Connection,
    attached_stream: AttachedStream,
    attached_connection: AttachedConnection,
    inbox_sender: &Sender<RuntimeEvent>,
    live_setting: Option<OtherUsersSetting>,
    served_connection: &ServedConnection,
) {
    let AttachedStream {
        client_id,
        deliveries,
        writer_handoff,
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
    let writer_inbox_sender = inbox_sender.clone();
    let paint_reporter = deliveries.create_paint_reporter();
    ending_notice.record_writer_started();
    std::thread::spawn(move || {
        let mut image_cache = ConnectionImageCache::new();
        let mut painted_update_sender = PaintedUpdateSender::new();
        let mut paint_window = PaintWindow::new();
        let should_close_read_direction = loop {
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
                    if send_waiting_host_writes(&mut frame_writer, &writer_handoff).is_ok() {
                        let _ = frame_writer.send(&terminal_event);
                    }
                }
                break false;
            }
            if send_waiting_host_writes(&mut frame_writer, &writer_handoff).is_err() {
                break true;
            }
            // The next `Pong` is due `heartbeat_interval` after the last frame
            // that went out. A `Pong` that is due goes out before the next
            // delivery is taken.
            let pong_due_at = heartbeat_interval.map(|heartbeat_interval| {
                frame_writer.get_last_frame_written_at() + heartbeat_interval
            });
            let paced_receive =
                if pong_due_at.is_some_and(|pong_due_at| pong_due_at <= Instant::now()) {
                    PacedReceive::TimedOut
                } else {
                    deliveries.recv_paced(
                        pong_due_at,
                        paint_window.can_send_update(frame_writer.get_written_byte_count()),
                        &ending_notice,
                    )
                };
            let delivery = match paced_receive {
                PacedReceive::Delivery(delivery) => delivery,
                PacedReceive::PaintedUpdateReported(painted_update_report) => {
                    if let Err(paint_report_error) =
                        paint_window.record_update_painted(painted_update_report)
                    {
                        tracing::warn!(
                            %client_id,
                            %paint_report_error,
                            "paint report refused"
                        );
                        break true;
                    }
                    continue;
                }
                // The top of the next turn writes the goodbye.
                PacedReceive::SessionEnding => continue,
                // No frame went out for `heartbeat_interval`.
                PacedReceive::TimedOut => {
                    if frame_writer.send(&SessionEvent::Pong).is_err() {
                        break true;
                    }
                    continue;
                }
                // The queue closed, which the server does when it detaches this
                // client. The wait hands back everything queued before the
                // close, held frames included: the goodbye follows the events
                // that preceded it.
                PacedReceive::Closed => {
                    if let Some(detach_goodbye) = build_detach_goodbye(&writer_is_link_lost) {
                        if send_waiting_host_writes(&mut frame_writer, &writer_handoff).is_ok() {
                            let _ = frame_writer.send(&detach_goodbye);
                        }
                    }
                    break false;
                }
            };
            if writer_handoff.take_owed_frame_mark() {
                let _ = writer_intake
                    .hand_over_event(&writer_inbox_sender, RuntimeEvent::QueueRoomFreed);
            }
            let write_started_at = Instant::now();
            let previous_update_sequence = painted_update_sender.get_last_update_sequence();
            let send_result = match &delivery {
                Delivery::Frame(render_snapshot) => send_painted_frame(
                    &mut frame_writer,
                    &mut image_cache,
                    &mut painted_update_sender,
                    render_snapshot,
                    graphics_capabilities,
                    client_id,
                )
                .map(|()| {
                    record_sent_painted_update(
                        &mut paint_window,
                        &painted_update_sender,
                        previous_update_sequence,
                        &frame_writer,
                        write_started_at,
                    );
                }),
                // The wake is read. The top of the next turn writes the
                // waiting bytes.
                Delivery::HostWrite => {
                    writer_handoff.clear_queued_wake();
                    continue;
                }
                _ => Ok(()),
            };
            if send_result.is_err() {
                break true;
            }
            if matches!(delivery, Delivery::Frame(_)) {
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
            // A resync goes out as `Resync`, then the frame it carries.
            if let Delivery::Snapshot {
                render_snapshot, ..
            } = &delivery
            {
                if send_painted_frame(
                    &mut frame_writer,
                    &mut image_cache,
                    &mut painted_update_sender,
                    render_snapshot,
                    graphics_capabilities,
                    client_id,
                )
                .is_err()
                {
                    break true;
                }
                record_sent_painted_update(
                    &mut paint_window,
                    &painted_update_sender,
                    previous_update_sequence,
                    &frame_writer,
                    write_started_at,
                );
            }
        };
        // The queue's receiver goes before the wake clears. A host write that
        // the bus adds after this queues a new wake, finds the receiver gone,
        // and removes this client's subscriber.
        drop(deliveries);
        writer_handoff.clear_queued_wake();
        // A failed write or a refused paint report closes the read direction:
        // the reading loop then ends on a failed read and hands over the
        // detach.
        if should_close_read_direction {
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
            IpcRequestKind::Keyboard { key_inputs } => RuntimeEvent::ClientKeyboard {
                client_id,
                key_inputs,
            },
            IpcRequestKind::Resize { viewport_size, .. }
                if viewport_size.compute_cell_count() > MAX_VIEWPORT_CELL_COUNT =>
            {
                tracing::debug!(
                    %client_id,
                    column_count = viewport_size.column_count,
                    row_count = viewport_size.row_count,
                    "a resize over the viewport cell limit"
                );
                continue;
            }
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
            IpcRequestKind::SubmitCommand {
                command_envelope,
                fired_action,
            } => {
                // The result is answered by the next painted frame. The reply
                // channel's receiving end drops here, and the dispatcher's send
                // into it fails.
                let (response_sender, _) = mpsc::channel();
                RuntimeEvent::Ipc {
                    command_envelope: Box::new(stamp_client_command_source(
                        *command_envelope,
                        client_id,
                    )),
                    fired_action,
                    response_sender,
                }
            }
            IpcRequestKind::SetPanePlacementPreview { placement_target } => {
                RuntimeEvent::SetPanePlacementPreview {
                    client_id,
                    placement_target,
                }
            }
            IpcRequestKind::ConfirmPanePlacement {
                command_id,
                source_pane_id,
                placement_target,
                expected_placed_tab_layout,
                fired_action,
            } => RuntimeEvent::ConfirmPanePlacement {
                client_id,
                command_id,
                source_pane_id,
                placement_target,
                expected_placed_tab_layout: Box::new(expected_placed_tab_layout),
                fired_action,
            },
            IpcRequestKind::Ping => continue,
            IpcRequestKind::PaintedUpdate { update_sequence } => {
                paint_reporter.report_painted_update(PaintedUpdateReport {
                    update_sequence,
                    reported_at: Instant::now(),
                });
                continue;
            }
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
            | IpcRequestKind::ClipboardCopy { .. }
            | IpcRequestKind::Prepare(_)
            | IpcRequestKind::Commit => break,
        };
        if served_connection
            .intake
            .hand_over_event(inbox_sender, runtime_event)
            .is_err()
        {
            break;
        }
    }
    drop(attached_connection);
    let _ = served_connection.intake.hand_over_event(
        inbox_sender,
        RuntimeEvent::ClientDetached {
            client_id,
            detached_at: SystemTime::now(),
            is_streamed: true,
            is_returning,
        },
    );
}

/// One image whose pixels this connection's client holds.
struct HeldConnectionImage {
    /// The identity the client holds the pixels under.
    image_content_id: u64,
    /// The record the pixels were sent from. While the entry lives, no other
    /// image takes the address of its decoded pixels.
    #[expect(dead_code)]
    image_record: Arc<ImageRecord>,
}

/// The images one painted frame shows with their pixels.
#[derive(Default)]
struct ShownImages {
    /// The RGBA byte count of each shown image, by the address of its decoded
    /// pixels.
    image_byte_count_by_address: HashMap<usize, u64>,
    /// Each animation a shown image plays, in placement order, each once.
    animations: Vec<Arc<DecodedAnimation>>,
}

/// The images a client keeps, chosen one by one while their RGBA bytes stay
/// within [`MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT`].
#[derive(Default)]
struct ImageBudget {
    /// The address of each kept image's decoded pixels.
    kept_image_addresses: HashSet<usize>,
    /// The RGBA bytes of every kept image.
    kept_byte_count: u64,
}

impl ImageBudget {
    /// Keep the image at `image_address`, whose RGBA bytes are
    /// `image_byte_count`, when it is kept already or fits with the kept
    /// images. Returns whether it is kept.
    fn keep_image(&mut self, image_address: usize, image_byte_count: u64) -> bool {
        if self.kept_image_addresses.contains(&image_address) {
            return true;
        }
        match self.kept_byte_count.checked_add(image_byte_count) {
            Some(kept_byte_count) if kept_byte_count <= MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT => {
                self.kept_byte_count = kept_byte_count;
                self.kept_image_addresses.insert(image_address);
                true
            }
            _ => false,
        }
    }

    /// Keep every frame of `animation` when the frames not kept already fit
    /// with the kept images, and keep none of them otherwise.
    fn keep_animation(&mut self, animation: &DecodedAnimation) {
        let mut new_frame_byte_count_by_address = HashMap::new();
        for animation_frame in animation.list_frames() {
            let frame_image = animation_frame.get_decoded_image();
            let frame_address = std::ptr::from_ref(frame_image).addr();
            if !self.kept_image_addresses.contains(&frame_address) {
                new_frame_byte_count_by_address
                    .insert(frame_address, compute_image_byte_count(frame_image));
            }
        }
        let kept_byte_count = new_frame_byte_count_by_address
            .values()
            .try_fold(self.kept_byte_count, |kept_byte_count, frame_byte_count| {
                kept_byte_count.checked_add(*frame_byte_count)
            })
            .filter(|kept_byte_count| *kept_byte_count <= MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT);
        if let Some(kept_byte_count) = kept_byte_count {
            self.kept_byte_count = kept_byte_count;
            self.kept_image_addresses
                .extend(new_frame_byte_count_by_address.into_keys());
        }
    }
}

/// The image records one attached connection's client holds.
///
/// The client keeps each record the session sends until an
/// `ImageContentReleased` names it or an `ImageCacheReset` arrives. The
/// session keeps, in this order and while their RGBA bytes stay within
/// [`MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT`]: the images the last painted frame
/// shows, and every frame of each animation those images play, an animation
/// whole or not at all. Every other record the client holds is released.
struct ConnectionImageCache {
    /// Every image whose pixels the client holds, by the address of its
    /// decoded pixels.
    held_image_by_address: HashMap<usize, HeldConnectionImage>,
    /// The images the last painted frame shows with their pixels.
    frame_shown_images: ShownImages,
    /// The next nonzero connection-local content identity.
    next_image_content_id: u64,
    /// Whether the next successful frame must invalidate the client's records.
    needs_image_cache_reset: bool,
}

impl ConnectionImageCache {
    /// Build an empty cache whose first image identity is 1.
    fn new() -> Self {
        Self {
            held_image_by_address: HashMap::new(),
            frame_shown_images: ShownImages::default(),
            next_image_content_id: 1,
            needs_image_cache_reset: false,
        }
    }

    /// Prepare `render_snapshot` for this connection.
    ///
    /// Each image of the frame, in placement order, is shown when it fits with
    /// the frame's earlier images, and travels without its record otherwise. A shown image the client does not hold gets the next
    /// content identity and an upload. Every held record the client no longer
    /// keeps is listed for release.
    fn prepare_image_frame(&mut self, render_snapshot: &RenderSnapshot) -> PreparedImageFrame {
        let image_placements: Vec<&ImagePlacementSnapshot> = render_snapshot
            .pane_snapshots
            .iter()
            .flat_map(|pane_snapshot| &pane_snapshot.image_placement_snapshots)
            .collect();
        let should_reset_image_cache = self.reset_image_cache_when_due(&image_placements);
        let mut image_budget = ImageBudget::default();
        let frame_shown_images = select_shown_images(&mut image_budget, &image_placements);
        let image_uploads = self.hold_shown_images(&frame_shown_images, &image_placements);
        self.frame_shown_images = frame_shown_images;
        let released_image_content_ids = self.release_unkept_images();
        let painted_frame =
            build_wire_frame_with_content_ids(render_snapshot, |_, image_placement| {
                self.find_image_content_id(image_placement)
            });
        PreparedImageFrame {
            should_reset_image_cache,
            released_image_content_ids,
            painted_frame,
            image_uploads,
        }
    }

    /// Forget every held image, start content identities at 1 when
    /// `image_placements` name more images the client does not hold than
    /// identities are left or a reset is due, and return whether it did.
    fn reset_image_cache_when_due(&mut self, image_placements: &[&ImagePlacementSnapshot]) -> bool {
        let new_image_addresses: HashSet<usize> = image_placements
            .iter()
            .filter_map(|image_placement| image_placement.get_image_record())
            .map(get_image_address)
            .filter(|image_address| !self.held_image_by_address.contains_key(image_address))
            .collect();
        let available_image_content_count = if self.next_image_content_id == 0 {
            0
        } else {
            u64::MAX - self.next_image_content_id + 1
        };
        let should_reset_image_cache = self.needs_image_cache_reset
            || u64::try_from(new_image_addresses.len()).map_or(true, |new_image_count| {
                new_image_count > available_image_content_count
            });
        if should_reset_image_cache {
            self.clear_image_cache();
            self.needs_image_cache_reset = false;
        }
        should_reset_image_cache
    }

    /// Give each image of `shown_images` that the client does not hold the
    /// next content identity, hold it, and return the uploads in placement
    /// order of `image_placements`.
    fn hold_shown_images(
        &mut self,
        shown_images: &ShownImages,
        image_placements: &[&ImagePlacementSnapshot],
    ) -> Vec<(u64, Arc<ImageRecord>)> {
        let mut image_uploads = Vec::new();
        for image_placement in image_placements {
            let Some(image_record) = image_placement.clone_image_record() else {
                continue;
            };
            let image_address = get_image_address(&image_record);
            if !shown_images
                .image_byte_count_by_address
                .contains_key(&image_address)
                || self.held_image_by_address.contains_key(&image_address)
            {
                continue;
            }
            let image_content_id = self.next_image_content_id;
            self.next_image_content_id = self.next_image_content_id.checked_add(1).unwrap_or(0);
            image_uploads.push((image_content_id, Arc::clone(&image_record)));
            self.held_image_by_address.insert(
                image_address,
                HeldConnectionImage {
                    image_content_id,
                    image_record,
                },
            );
        }
        image_uploads
    }

    /// Release every held image the client no longer keeps, and return their
    /// content identities in ascending order.
    ///
    /// The client keeps, in this order and within
    /// [`MAX_FRAME_IMAGE_TRANSFER_BYTE_COUNT`]: the images the last painted
    /// frame shows, and the frames of each animation those images play, an
    /// animation whole or not at all.
    fn release_unkept_images(&mut self) -> Vec<u64> {
        let mut image_budget = ImageBudget::default();
        keep_shown_images(&mut image_budget, &self.frame_shown_images);
        for animation in &self.frame_shown_images.animations {
            image_budget.keep_animation(animation);
        }
        let mut released_image_content_ids = Vec::new();
        self.held_image_by_address
            .retain(|image_address, held_image| {
                let is_kept = image_budget.kept_image_addresses.contains(image_address);
                if !is_kept {
                    released_image_content_ids.push(held_image.image_content_id);
                }
                is_kept
            });
        released_image_content_ids.sort_unstable();
        released_image_content_ids
    }

    /// The content identity `image_placement` travels with on this
    /// connection: the identity of its held image, `None` for an image the
    /// client does not hold, and its own identity when it has no record.
    fn find_image_content_id(&self, image_placement: &ImagePlacementSnapshot) -> Option<u64> {
        match image_placement.get_image_record() {
            Some(image_record) => self
                .held_image_by_address
                .get(&get_image_address(image_record))
                .map(|held_image| held_image.image_content_id),
            None => Some(image_placement.get_image_content_id()),
        }
    }

    /// Forget every held image, start identities at 1 again, and mark the
    /// next frame to reset the client's image records.
    fn clear_image_cache(&mut self) {
        self.held_image_by_address.clear();
        self.frame_shown_images = ShownImages::default();
        self.next_image_content_id = 1;
        self.needs_image_cache_reset = true;
    }
}

/// Keep each image of `shown_images` in `image_budget`.
fn keep_shown_images(image_budget: &mut ImageBudget, shown_images: &ShownImages) {
    for (image_address, image_byte_count) in &shown_images.image_byte_count_by_address {
        image_budget.keep_image(*image_address, *image_byte_count);
    }
}

/// The images of `image_placements` that `image_budget` keeps, chosen in
/// placement order, and each animation they play.
fn select_shown_images(
    image_budget: &mut ImageBudget,
    image_placements: &[&ImagePlacementSnapshot],
) -> ShownImages {
    let mut shown_images = ShownImages::default();
    let mut shown_animation_addresses = HashSet::new();
    for image_placement in image_placements {
        let Some(image_record) = image_placement.get_image_record() else {
            continue;
        };
        let image_address = get_image_address(image_record);
        let image_byte_count = compute_image_byte_count(&image_record.image);
        if !image_budget.keep_image(image_address, image_byte_count) {
            continue;
        }
        shown_images
            .image_byte_count_by_address
            .insert(image_address, image_byte_count);
        if let Some(animation) = &image_record.animation {
            if shown_animation_addresses.insert(Arc::as_ptr(animation).addr()) {
                shown_images.animations.push(Arc::clone(animation));
            }
        }
    }
    shown_images
}

/// The address of `image_record`'s decoded pixels.
fn get_image_address(image_record: &ImageRecord) -> usize {
    Arc::as_ptr(&image_record.image).addr()
}

/// The RGBA byte count of `decoded_image`.
fn compute_image_byte_count(decoded_image: &DecodedImage) -> u64 {
    u64::try_from(decoded_image.rgba_bytes.len()).unwrap_or(u64::MAX)
}

/// A painted frame plus the image records its connection still needs.
struct PreparedImageFrame {
    /// Whether the client must discard every record before this frame.
    should_reset_image_cache: bool,
    /// The records the client drops after this frame, in ascending order.
    released_image_content_ids: Vec<u64>,
    /// Placement geometry and connection-local content identities.
    painted_frame: PaintedFrame,
    /// New content identities and the records uploaded under them.
    image_uploads: Vec<(u64, Arc<ImageRecord>)>,
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

/// Take every host-write byte waiting in `writer_handoff` and send them as
/// one host write, as [`send_host_write`] does. Sends nothing when no byte
/// waits.
///
/// # Errors
/// The [`IpcError`] of the first frame that could not be written.
fn send_waiting_host_writes(
    frame_writer: &mut FrameWriter,
    writer_handoff: &WriterHandoff,
) -> Result<(), IpcError> {
    let host_write_bytes = writer_handoff.take_host_write_bytes();
    if host_write_bytes.is_empty() {
        return Ok(());
    }
    send_host_write(frame_writer, &host_write_bytes)
}

/// Record in `paint_window` the update that `painted_update_sender` built
/// last, when it built one after `previous_update_sequence`. The update's
/// first byte was written at `write_started_at`, and `frame_writer` wrote its
/// last byte, and the image transfers that follow it, just before.
fn record_sent_painted_update(
    paint_window: &mut PaintWindow,
    painted_update_sender: &PaintedUpdateSender,
    previous_update_sequence: u64,
    frame_writer: &FrameWriter,
    write_started_at: Instant,
) {
    let last_update_sequence = painted_update_sender.get_last_update_sequence();
    if last_update_sequence == previous_update_sequence {
        return;
    }
    paint_window.record_update_sent(
        last_update_sequence,
        frame_writer.get_written_byte_count(),
        write_started_at,
        Instant::now(),
    );
}

/// Send one painted update and each new image record after it.
///
/// In this order: an `ImageCacheReset` when one is due, the update, the
/// `ImageContentReleased` frames of the records the client no longer keeps,
/// and the records the update shows that the client does not hold, as
/// [`ConnectionImageCache::prepare_image_frame`] chooses them.
///
/// The update turns what `painted_update_sender` sent last into
/// `render_snapshot`, as [`PaintedUpdateSender::build_update_frames`] builds
/// it. An update that changes nothing is not sent, unless it follows an
/// image cache reset or image records follow it. An update over the frame
/// cap is not sent, and no release or record follows it.
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
    painted_update_sender: &mut PaintedUpdateSender,
    render_snapshot: &RenderSnapshot,
    graphics_capabilities: GraphicsCapabilities,
    client_id: ClientId,
) -> Result<(), IpcError> {
    if !graphics_capabilities.has_native_image_protocol() {
        let mut painted_frame = build_wire_frame(render_snapshot);
        for pane_snapshot in &mut painted_frame.pane_snapshots {
            hide_image_records(&mut pane_snapshot.image_placement_snapshots);
        }
        return send_painted_update(
            frame_writer,
            image_cache,
            painted_update_sender,
            render_snapshot,
            &painted_frame,
            false,
            client_id,
        )
        .map(|_| ());
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
    let is_update_required = prepared_image_frame.should_reset_image_cache
        || !prepared_image_frame.image_uploads.is_empty();
    let is_update_held = send_painted_update(
        frame_writer,
        image_cache,
        painted_update_sender,
        render_snapshot,
        &prepared_image_frame.painted_frame,
        is_update_required,
        client_id,
    )?;
    if !is_update_held {
        return Ok(());
    }
    send_image_releases(
        frame_writer,
        image_cache,
        client_id,
        &prepared_image_frame.released_image_content_ids,
    )?;
    send_image_uploads(
        frame_writer,
        image_cache,
        client_id,
        &mut |frame_writer, image_cache| {
            send_painted_update(
                frame_writer,
                image_cache,
                painted_update_sender,
                render_snapshot,
                &prepared_image_frame.painted_frame,
                true,
                client_id,
            )
        },
        "the painted frame",
        prepared_image_frame.image_uploads,
    )
}

/// Build the painted update that turns what `painted_update_sender` sent last
/// into `render_snapshot` and `painted_frame`, and send its parts as binary
/// frames. An update that changes nothing sends no frame, unless
/// `is_update_required` is `true`.
///
/// Returns `true` when the client holds the update: its parts went out, or
/// it changed nothing. Returns `false` when the update does not fit the frame
/// cap: it is logged and not sent, and `image_cache` is cleared.
///
/// # Errors
/// The [`IpcError`] of a write that left the connection unusable.
fn send_painted_update(
    frame_writer: &mut FrameWriter,
    image_cache: &mut ConnectionImageCache,
    painted_update_sender: &mut PaintedUpdateSender,
    render_snapshot: &RenderSnapshot,
    painted_frame: &PaintedFrame,
    is_update_required: bool,
    client_id: ClientId,
) -> Result<bool, IpcError> {
    let part_payloads = match painted_update_sender.build_update_frames(
        render_snapshot,
        painted_frame,
        is_update_required,
    ) {
        Ok(Some(part_payloads)) => part_payloads,
        Ok(None) => return Ok(true),
        Err(encode_error) => {
            tracing::warn!(
                %client_id,
                %encode_error,
                "painted update exceeded the frame cap"
            );
            image_cache.clear_image_cache();
            return Ok(false);
        }
    };
    for part_payload in part_payloads {
        if let Err(write_error) = frame_writer.send_binary(&part_payload) {
            tracing::warn!(%client_id, %write_error, "painted update write failed");
            return Err(write_error);
        }
    }
    Ok(true)
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

/// Send `released_image_content_ids` as `ImageContentReleased` frames of at
/// most [`MAX_RELEASED_IMAGE_CONTENT_COUNT`] identities each, in order. No
/// identity sends no frame.
///
/// # Errors
/// The [`IpcError`] of a write that left the connection unusable, as
/// [`report_image_send_error`] sorts it.
fn send_image_releases(
    frame_writer: &mut FrameWriter,
    image_cache: &mut ConnectionImageCache,
    client_id: ClientId,
    released_image_content_ids: &[u64],
) -> Result<(), IpcError> {
    for released_image_content_id_chunk in
        released_image_content_ids.chunks(MAX_RELEASED_IMAGE_CONTENT_COUNT)
    {
        let image_release_event = SessionEvent::ImageContentReleased {
            image_content_ids: released_image_content_id_chunk.to_vec(),
        };
        if let Err(write_error) = frame_writer.send(&image_release_event) {
            return report_image_send_error(
                image_cache,
                client_id,
                write_error,
                "an image release",
            );
        }
    }
    Ok(())
}

/// Send each record in `image_uploads` as one `ImageContentStart` followed by
/// its image chunk frames ([`koshi_ipc::image_chunk`]). Each chunk frame
/// writes its head, then the chunk straight from the record's RGBA bytes
/// ([`FrameWriter::send_binary_with_head`]). The caller sent the frames that lead the
/// records. `send_leading_frames` sends them again before every
/// `MAX_FRAME_IMAGE_TRANSFER_COUNT`-th record: 9 records with a limit of 4 send
/// records 0-3, the leading frames, records 4-7, the leading frames, record 8.
/// `send_leading_frames` returns `false` when the leading frames did not go
/// out and the connection survives, which stops the sending and returns `Ok`.
/// A failed record write logs `an image transfer start after
/// <leading_event_part>` or `an image transfer chunk after <leading_event_part>`.
/// A record write the connection survives, a frame over the cap, stops the
/// sending and returns `Ok`.
///
/// # Errors
/// The [`IpcError`] of a write that left the connection unusable, as
/// [`report_image_send_error`] sorts it.
fn send_image_uploads(
    frame_writer: &mut FrameWriter,
    image_cache: &mut ConnectionImageCache,
    client_id: ClientId,
    send_leading_frames: &mut dyn FnMut(
        &mut FrameWriter,
        &mut ConnectionImageCache,
    ) -> Result<bool, IpcError>,
    leading_event_part: &str,
    image_uploads: Vec<(u64, Arc<ImageRecord>)>,
) -> Result<(), IpcError> {
    for (upload_index, (image_content_id, image_record)) in image_uploads.into_iter().enumerate() {
        if upload_index != 0
            && upload_index % MAX_FRAME_IMAGE_TRANSFER_COUNT == 0
            && !send_leading_frames(frame_writer, image_cache)?
        {
            return Ok(());
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
            let image_chunk_frame_head =
                build_image_chunk_frame_head(image_content_id, byte_offset, is_last_chunk);
            if let Err(write_error) =
                frame_writer.send_binary_with_head(&image_chunk_frame_head, chunk_bytes)
            {
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

/// What a check of the watch `image_replacement` holds runs once the program
/// file prints another koshi version: [`restart_into_installed_version`] with
/// that version, through `connection_intake` and `inbox_sender`.
fn build_installed_version_restart(
    connection_intake: &Arc<Intake>,
    inbox_sender: &Sender<RuntimeEvent>,
    image_replacement: &Arc<ImageReplacement>,
) -> impl FnOnce(String) + Send + 'static {
    let restart_intake = Arc::clone(connection_intake);
    let restart_inbox_sender = inbox_sender.clone();
    let restart_image_replacement = Arc::clone(image_replacement);
    move |installed_version| {
        restart_into_installed_version(
            &restart_intake,
            &restart_inbox_sender,
            &restart_image_replacement,
            &installed_version,
        );
    }
}

/// Restart this session into its program file, which now holds koshi
/// `installed_version`. The restart check `image_replacement` holds runs on
/// this thread first.
///
/// A check that refuses is logged at warning level with the sentence it
/// refused with, and nothing reaches the dispatcher. That refusal calls
/// [`ExecutableWatch::schedule_restart_retry`] on the watch when
/// [`RestartRefusal::can_end_without_file_change`] holds for it. A check that
/// passes asks the dispatcher through [`RuntimeEvent::IpcRestart`], and a
/// dispatcher that takes the restart is logged at info level. An intake that
/// refuses requests takes nothing.
fn restart_into_installed_version(
    connection_intake: &Intake,
    inbox_sender: &Sender<RuntimeEvent>,
    image_replacement: &ImageReplacement,
    installed_version: &str,
) {
    if let Err(restart_refusal) = (image_replacement.restart_check)() {
        tracing::warn!(
            installed_version,
            %restart_refusal,
            "the program file holds another koshi version, and the session cannot restart into it"
        );
        if restart_refusal.can_end_without_file_change() {
            image_replacement.executable_watch.schedule_restart_retry();
        }
        return;
    }
    let dispatch_answer =
        request_dispatcher_answer(connection_intake, inbox_sender, |response_sender| {
            RuntimeEvent::IpcRestart { response_sender }
        });
    if dispatch_answer.is_ok() {
        tracing::info!(
            installed_version,
            "the program file holds another koshi version; the session restarts into it"
        );
    }
}

/// The answer that refuses the request `request_id` with
/// [`IpcErrorCode::RequestFailed`] and `message`.
fn build_request_failed_response(request_id: Option<u64>, message: String) -> IpcResponse {
    IpcResponse {
        request_id,
        answer_result: IpcResult::Error(IpcErrorPayload {
            code: IpcErrorCode::RequestFailed,
            message,
        }),
    }
}

/// Hand one request to the dispatcher thread and wait for its answer: build
/// the inbox event around a fresh reply channel, hand it over the intake, and
/// block on the reply.
///
/// # Errors
/// The [`DispatchFailure`] of the hand-over, and
/// [`DispatchFailure::DispatcherGone`] when the dispatcher drops the reply
/// channel without an answer.
fn request_dispatcher_answer<DispatcherResponse>(
    connection_intake: &Intake,
    inbox_sender: &Sender<RuntimeEvent>,
    build_runtime_event: impl FnOnce(mpsc::Sender<DispatcherResponse>) -> RuntimeEvent,
) -> Result<DispatcherResponse, DispatchFailure> {
    let (response_sender, response_receiver) = mpsc::channel();
    connection_intake.hand_over_event(inbox_sender, build_runtime_event(response_sender))?;
    response_receiver
        .recv()
        .map_err(|_| DispatchFailure::DispatcherGone)
}

/// The dispatcher's answer to the request `request_id` read on `connection`,
/// as [`request_dispatcher_answer`] gives it. `None` means the connection
/// ends: the dispatcher is gone and nothing is written, or the intake refuses
/// requests and `connection` carries the refusal of `request_id` with
/// [`IpcErrorCode::Restarting`] and [`SESSION_RESTARTING_MESSAGE`].
fn request_dispatcher_response<DispatcherResponse>(
    connection_intake: &Intake,
    inbox_sender: &Sender<RuntimeEvent>,
    connection: &mut Connection,
    request_id: Option<u64>,
    build_runtime_event: impl FnOnce(mpsc::Sender<DispatcherResponse>) -> RuntimeEvent,
) -> Option<DispatcherResponse> {
    match request_dispatcher_answer(connection_intake, inbox_sender, build_runtime_event) {
        Ok(dispatcher_response) => Some(dispatcher_response),
        Err(DispatchFailure::Refused) => {
            let restarting_refusal = IpcResponse {
                request_id,
                answer_result: IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::Restarting,
                    message: SESSION_RESTARTING_MESSAGE.to_owned(),
                }),
            };
            let _ = connection.send(&restarting_refusal);
            None
        }
        Err(DispatchFailure::DispatcherGone) => None,
    }
}

#[cfg(test)]
mod tests;
