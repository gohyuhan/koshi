//! The router process: one per user, owning the list of running sessions.
//!
//! [`run_router`](crate::router::run_router) takes the advisory lock on the
//! router lock file, binds the router's control socket, writes the endpoint
//! file advertising it, rebuilds the session list by probing what is already
//! running, and then serves control-plane requests until no session is left.
//!
//! The router is the parent of every session server it starts. It hands a
//! caller a session's control-socket address and steps out: pane traffic runs
//! between the caller and that session server directly, never through here.
//!
//! One thread accepts connections, serves only this user's own, and gives each
//! its own serving thread; a serving thread only holds a channel sender, so
//! the session list has a single owner — the dispatcher loop on the main
//! thread. A session that dies leaves the list three ways: its reaper thread
//! reports the child's exit, on Unix a watcher thread reports the exit of a
//! session the rebuild picked up, or a lookup finds nothing listening at its
//! address. All remove the entry and, for a session this user started, the
//! files it left behind.
//!
//! A restart request is answered first and acted on second: the router sends
//! the `Restarting` reply, then restarts into the binary on disk. On Unix it
//! replaces its own running image and keeps the same process id; a restart
//! that fails resumes serving, still ignoring the SIGPIPE signal. Every
//! serving thread blocks SIGPIPE on its own mask; a write to a peer that
//! hung up returns an error in every disposition state. On Windows it starts
//! the new binary, which waits for the router lock, and then runs its own
//! shutdown and exits.
//!
//! With no session left, the dispatcher waits one idle window for a request
//! and exits when none arrives. A caller that needs the router again starts
//! it: connect, and on failure spawn the router and retry.
//!
//! The router also opens the machine's TLS port for remote clients, when
//! `koshi.kdl` names an address and the operator has switched remote access
//! on. The remote listener holds those connections and asks the dispatcher
//! what each caller's secret reaches; the dispatcher keeps the socket of every
//! connection it admitted, so a revoked or replaced secret ends its
//! connections at once.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::net::{Shutdown, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use fs4::{FileExt, TryLockError};
use koshi_config::layer::merge_server;
use koshi_config::types::ServerConfig;
use koshi_core::ids::SessionId;
use koshi_core::naming::{generate_name, NameKind};
use koshi_ipc::endpoint::{
    compute_socket_address, remove_socket_file, resolve_resume_file_path, EndpointFile,
};
use koshi_ipc::error::{IpcError, RemoteFile};
use koshi_ipc::plane::{self, RequestDisposition};
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::remote_state::{
    is_remote_enabled, CertFile, EnabledFile, CERT_FILE_FORMAT, ENABLED_FILE_FORMAT,
};
use koshi_ipc::remote_tokens::{
    hash_connection_token, resolve_token_store_path, TokenScope, TokenStore,
};
use koshi_ipc::remote_wire::RemoteSessionRow;
use koshi_ipc::router::{
    compute_router_socket_address, resolve_router_endpoint_path, resolve_router_lock_path,
    ControlPlane, RouterHandshake, RouterRequestKind, RouterResponse, RouterResult, SessionAddress,
    SessionSelector, SessionServerReady, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::tls;
use koshi_ipc::transport::{self, Connection, Listener};
use koshi_ipc::validate::{reclaim_stale_socket, validate_socket_address};

use koshi_link::error::CliError;
use koshi_link::ipc_client;
use koshi_link::router_client::{ROUTER_SUBCOMMAND, RUNTIME_DIRECTORY_FLAG};
use koshi_runtime::server::is_binary_runnable;

use crate::process;
use crate::remote_listener::{self, AdmissionAsk, Admitted, WarningRateLimiter};
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

/// How long the accept loop pauses after a failed accept before trying
/// again, so a persistent accept error cannot spin a core.
const ACCEPT_RETRY_DELAY_DURATION: Duration = Duration::from_millis(100);

/// How long a replacement router waits for the previous router to release the
/// router lock. The operating system releases that lock if the previous router
/// dies.
const LOCK_HANDOVER_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

/// How long the lock wait pauses between attempts on the router lock.
const LOCK_HANDOVER_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(100);

/// How long shutdown pauses after withdrawing the socket, giving a serving
/// thread time to finish the reply it is writing.
const DRAIN_GRACE_DURATION: Duration = Duration::from_millis(100);

/// The flag carrying a `--profile` name to the session server the router
/// starts, so the session opens that profile's tabs and panes.
const PROFILE_FLAG: &str = "--profile";

/// The flag this router passes to the router it starts, telling that one to
/// wait for the router lock rather than yield to the router holding it.
#[cfg(windows)]
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
    /// A session server the router started has exited.
    ChildExited(SessionId),
    /// The `Restarting` reply has been written to its connection, so the
    /// router may now restart.
    RestartDelivered,
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
/// Each one keeps a socket handle in [`RemoteState::admitted_remote_connections`] and a thread in the
/// listener. A connection arriving over this count is refused in the sentence
/// every refusal carries, and nothing is registered for it.
pub(crate) const MAX_LIVE_REMOTE_CONNECTION_COUNT: usize = 128;

/// What the router holds for remote clients: where the listener binds, whether
/// it is open, and the connections it has admitted.
///
/// Owned by the dispatcher loop alone, as the session list is.
struct RemoteState {
    /// The address `koshi.kdl` names, or `None` when it names none.
    remote_listen_address: Option<String>,
    /// The koshi data directory holding the certificate and the record of the
    /// operator's yes, or `None` when this machine has none.
    data_directory: Option<PathBuf>,
    /// Whether the listener is open.
    listening: bool,
    /// The remote connections this machine has admitted, whether they have
    /// attached to a session or not. Never longer than [`MAX_LIVE_REMOTE_CONNECTION_COUNT`].
    admitted_remote_connections: Vec<AdmittedRemoteConnection>,
    /// The number the next admitted connection is registered under.
    next_remote_connection_id: u64,
    /// The warning written when the list is full.
    full_capacity_warning: WarningRateLimiter,
}

impl RemoteState {
    /// End every admitted connection a secret in `hashes` opened, and drop it
    /// from the list.
    ///
    /// Each connection's socket is shut down in both directions, ending the
    /// thread reading it and its two bridge threads when it has attached. A
    /// a new attach on a dropped record is refused.
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
    /// A `Restarting` reply reached its caller, so the router restarts into
    /// the binary on disk.
    Restart,
}

/// Run the router until no session is left.
///
/// Takes the advisory lock first: another router already holding it means
/// this call returns `Ok(())` having bound nothing, and the caller connects
/// to that router instead. `wait_for_lock` waits up to `LOCK_HANDOVER_TIMEOUT_DURATION`
/// for that router to release it, and yields the same way once the wait runs
/// out. With the lock held, the socket is bound, the endpoint file is written,
/// the session list is rebuilt from what is already running, and the
/// dispatcher serves requests until an idle window passes with no session
/// running.
///
/// A restart request ends the dispatcher and restarts this router into the
/// binary on disk; a restart that fails resumes the dispatcher with everything
/// the router holds untouched.
pub fn run_router(
    runtime_directory: &Path,
    wait_for_lock: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    koshi_paths::ensure_private_directory(runtime_directory)?;

    // The path this program was started from, read once here. A restart runs
    // the binary at this path.
    let executable_path = std::env::current_exe()?;

    // Where this machine's remote access tokens live, resolved once here. A
    // machine with no resolvable data directory has no store, so it holds no
    // remote access token.
    let data_directory = koshi_paths::resolve_data_directory();
    let token_store = data_directory.as_deref().map(resolve_token_store_path);

    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(resolve_router_lock_path(runtime_directory))?;
    if !take_router_lock(&lock_file, wait_for_lock)? {
        return Ok(());
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

    let mut registry = rebuild_session_registry(
        runtime_directory,
        ipc_client::resolve_shared_sessions_base_directory().as_deref(),
    );

    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let shutting_down = Arc::new(AtomicBool::new(false));
    let accept_thread = match start_router_accept_thread(
        listener,
        router_connection_token,
        router_events_sender.clone(),
        &shutting_down,
    ) {
        Ok(handle) => handle,
        Err(accept_thread_error) => {
            let _ = std::fs::remove_file(&endpoint_path);
            remove_socket_file(&router_socket_address);
            return Err(accept_thread_error.into());
        }
    };

    let mut remote_state = RemoteState {
        remote_listen_address: merge_server(
            ServerConfig::default(),
            koshi_link::config::load_app_layer().into_iter().collect(),
        )
        .remote_listen,
        data_directory,
        listening: false,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };
    open_remote_listener(&mut remote_state, &router_events_sender);

    #[cfg(unix)]
    for (session_id, session_entry) in &registry {
        if session_entry.process_id != 0 {
            watch_session_process_exit(
                session_entry.process_id,
                *session_id,
                router_events_sender.clone(),
            );
        }
    }

    loop {
        match run_dispatch_loop(
            runtime_directory,
            &executable_path,
            token_store.as_deref(),
            &router_events_sender,
            &router_events_receiver,
            ROUTER_IDLE_TIMEOUT_DURATION,
            &mut registry,
            &mut remote_state,
        ) {
            RouterExit::Idle => break,
            RouterExit::Restart => {
                #[cfg(unix)]
                {
                    // The call returns only when the exec failed, having put
                    // the SIGPIPE ignore back; the loop serves on.
                    let _ = restart_by_exec(&executable_path, runtime_directory);
                }
                #[cfg(windows)]
                // The new router waits for the lock this one drops last; a
                // spawn that failed leaves this router serving.
                if hand_over_router_to_next_process(&executable_path, runtime_directory).is_ok() {
                    break;
                }
            }
        }
    }

    shutting_down.store(true, Ordering::SeqCst);
    // The accept loop sits blocked in `accept`; a bare connection wakes it so
    // it observes the flag. The connection is held open across the join,
    // since on Windows a caller that drops before `accept` runs can leave
    // nothing for `accept` to return.
    if let Ok(wake_connection) = Connection::connect(&router_socket_address) {
        let _ = accept_thread.join();
        drop(wake_connection);
    }
    let _ = std::fs::remove_file(&endpoint_path);
    remove_socket_file(&router_socket_address);
    // A serving thread can stay blocked on its peer, so shutdown waits a fixed
    // moment instead. A caller that loses its last reply retries through the
    // same path as a router that has already exited.
    std::thread::sleep(DRAIN_GRACE_DURATION);

    drop(lock_file);
    Ok(())
}

/// Take the router lock. `true` means this process holds it, and `false` that
/// another router does.
///
/// Without `wait_for_lock` one attempt decides it. With `wait_for_lock` the
/// attempt is repeated every [`LOCK_HANDOVER_POLL_INTERVAL_DURATION`] for up to
/// [`LOCK_HANDOVER_TIMEOUT_DURATION`], and a wait that runs out reads as another router
/// holding it.
fn take_router_lock(lock_file: &File, wait_for_lock: bool) -> std::io::Result<bool> {
    let deadline = Instant::now() + LOCK_HANDOVER_TIMEOUT_DURATION;
    loop {
        match FileExt::try_lock(lock_file) {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) => {
                if !wait_for_lock || Instant::now() >= deadline {
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
    router_events_sender: &Sender<RouterEvent>,
) {
    let Some(remote_listen_address) = remote_state.remote_listen_address.clone() else {
        return;
    };
    let Some(data_directory) = remote_state
        .data_directory
        .clone()
        .filter(|data_directory| is_remote_enabled(data_directory))
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
        remote_listen_address.clone(),
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
    bound_listener.start_serving(router_events_sender.clone());
    remote_state.listening = true;
}

/// This machine's certificate and its fingerprint, generating one when there is
/// none to read.
///
/// The certificate koshi generates names `koshi`. A dialling client pins the
/// fingerprint of the certificate it was shown and checks nothing else about
/// it.
///
/// # Errors
/// [`IpcError::RemoteFileWrite`] naming [`RemoteFile::Certificate`] and what
/// failed, for a certificate that could not be generated or could not be
/// written.
fn load_or_create_certificate(data_directory: &Path) -> Result<(CertFile, String), IpcError> {
    let certificate_file_path = CertFile::resolve_certificate_file_path(data_directory);
    if let Ok(certificate_file) = CertFile::load_from_path(&certificate_file_path) {
        let certificate_fingerprint =
            tls::compute_certificate_fingerprint(&certificate_file.cert_der);
        return Ok((certificate_file, certificate_fingerprint));
    }
    let generated_certificate = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .map_err(|certificate_generation_error| IpcError::RemoteFileWrite {
            remote_file: RemoteFile::Certificate,
            remote_file_path: certificate_file_path.display().to_string(),
            error_detail: format!(
                "the certificate could not be generated: {certificate_generation_error}"
            ),
        })?;
    let certificate_file = CertFile {
        file_format: CERT_FILE_FORMAT,
        cert_der: generated_certificate.cert.der().to_vec(),
        key_der: generated_certificate.signing_key.serialize_der(),
    };
    certificate_file.write_to_path(&certificate_file_path)?;
    let certificate_fingerprint = tls::compute_certificate_fingerprint(&certificate_file.cert_der);
    Ok((certificate_file, certificate_fingerprint))
}

/// Replace this process's running image with the binary at `exe`, serving the
/// same runtime directory. The call returns only when the exec failed, and
/// hands back that error, on the terms
/// [`exec_and_keep_ignoring_sigpipe`](crate::process::exec_and_keep_ignoring_sigpipe)
/// states.
///
/// A successful exec closes the router lock file with every other descriptor
/// the standard library opened close-on-exec. The new image's [`run_router`]
/// then takes the lock, reclaims the socket path, binds, writes a fresh
/// endpoint file, and rebuilds the session list — under the same process id.
#[cfg(unix)]
fn restart_by_exec(executable_path: &Path, runtime_directory: &Path) -> std::io::Error {
    process::exec_and_keep_ignoring_sigpipe(
        std::process::Command::new(executable_path)
            .arg(ROUTER_SUBCOMMAND)
            .arg(RUNTIME_DIRECTORY_FLAG)
            .arg(runtime_directory),
    )
}

/// Start the binary at `exe` as a new router over the same runtime directory,
/// waiting for the lock this router still holds.
///
/// The new router is detached with a process group of its own and no console,
/// and its input and output go nowhere. An error means nothing was started.
#[cfg(windows)]
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
    shutting_down: &Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    let shutdown_flag = Arc::clone(shutting_down);
    std::thread::Builder::new()
        .name("koshi-router-accept".to_string())
        .spawn(move || {
            run_router_accept_loop(
                &listener,
                &router_connection_token,
                &router_events_sender,
                &shutdown_flag,
            )
        })
}

/// Accept connections until the shutdown flag is set, giving each its own
/// serving thread.
///
/// Only this router's own user is served. A connection opened by another user,
/// and one whose user cannot be read, is closed without being served.
fn run_router_accept_loop(
    listener: &Listener,
    router_connection_token: &ConnectionToken,
    router_events_sender: &Sender<RouterEvent>,
    shutting_down: &AtomicBool,
) {
    transport::accept_until_shutdown(
        listener,
        shutting_down,
        ACCEPT_RETRY_DELAY_DURATION,
        |connection| {
            // The OS reports which user opened the connection, so a peer cannot
            // claim to be another one.
            if !matches!(connection.is_peer_same_user(), Ok(true)) {
                return;
            }
            let router_connection_token = router_connection_token.clone();
            let router_events_sender = router_events_sender.clone();
            std::thread::spawn(move || {
                serve_router_connection(connection, router_connection_token, &router_events_sender)
            });
        },
    );
}

/// Serve one router connection until its peer hangs up or a fault closes it.
///
/// [`plane::next_request`] makes every decision that is the same on every
/// koshi protocol — the framing faults, a request kind this build does not
/// have, and the Hello. What is left crosses to the dispatcher and comes back
/// as its answer.
///
/// A `Restarting` answer that has been written is reported to the dispatcher,
/// and this connection keeps serving until the router ends.
///
/// On Unix the thread blocks SIGPIPE on its own signal mask first; a write to
/// a peer that hung up returns an error whatever the process-wide disposition
/// is.
fn serve_router_connection(
    mut connection: Connection,
    router_connection_token: ConnectionToken,
    router_events_sender: &Sender<RouterEvent>,
) {
    #[cfg(unix)]
    process::block_sigpipe_on_this_thread();
    let mut gate = RouterHandshake::from_connection_token(router_connection_token);
    loop {
        let (request_id, request_kind) = match plane::next_request::<ControlPlane>(
            &mut connection,
            &mut gate,
            BUILD_VERSION,
            &plane::is_always_admitted,
        ) {
            RequestDisposition::Answered => continue,
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
            let _ = router_events_sender.send(RouterEvent::RestartDelivered);
        }
    }
}

/// Hand one request to the dispatcher and wait for its answer. `None` means
/// the dispatcher is gone — the router is exiting — so the caller closes its
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
/// While a session is running the loop blocks for the next event. While none
/// is, it waits `idle_exit` for one: an event inside that window is served
/// and the loop goes on, and a window that passes ends the loop with
/// [`RouterExit::Idle`]. A delivered `Restarting` reply ends it with
/// [`RouterExit::Restart`] instead, so the caller restarts this router into
/// the binary at `exe`.
///
/// `router_events_sender` is the loop's own sender, handed to each session's reaper
/// thread so a child's exit reaches here. `token_store` is the remote access
/// token store every token request is answered against, and `remote_state` is what
/// the router holds for remote clients.
#[allow(clippy::too_many_arguments)]
fn run_dispatch_loop(
    runtime_directory: &Path,
    executable_path: &Path,
    token_store: Option<&Path>,
    router_events_sender: &Sender<RouterEvent>,
    router_events_receiver: &Receiver<RouterEvent>,
    idle_exit: Duration,
    registry: &mut SessionRegistry,
    remote_state: &mut RemoteState,
) -> RouterExit {
    loop {
        let received_event = if registry.is_empty() {
            router_events_receiver.recv_timeout(idle_exit).ok()
        } else {
            router_events_receiver.recv().ok()
        };
        let Some(event) = received_event else {
            return RouterExit::Idle;
        };
        match event {
            RouterEvent::Request {
                request_kind,
                response_sender,
            } => {
                let _ = response_sender.send(serve_router_request(
                    runtime_directory,
                    executable_path,
                    token_store,
                    registry,
                    remote_state,
                    router_events_sender,
                    request_kind,
                ));
            }
            RouterEvent::ChildExited(session_id) => {
                remove_session_from_registry(runtime_directory, registry, session_id)
            }
            RouterEvent::RestartDelivered => return RouterExit::Restart,
            RouterEvent::Admission(admission_question) => {
                serve_remote_admission(
                    runtime_directory,
                    token_store,
                    registry,
                    remote_state,
                    admission_question,
                );
            }
        }
    }
}

/// Answer one request against the session list.
///
/// The dispatcher answers one request at a time, so the remote access token
/// store at `token_store` has one writer.
fn serve_router_request(
    runtime_directory: &Path,
    executable_path: &Path,
    token_store: Option<&Path>,
    registry: &mut SessionRegistry,
    remote_state: &mut RemoteState,
    router_events_sender: &Sender<RouterEvent>,
    request_kind: RouterRequestKind,
) -> RouterResult {
    match request_kind {
        RouterRequestKind::Hello { .. } => {
            unreachable!("Hello is answered by the connection thread before dispatch")
        }
        RouterRequestKind::CreateSession {
            profile,
            working_directory,
            is_other_user_access_allowed,
        } => create_session(
            runtime_directory,
            registry,
            router_events_sender,
            profile.as_deref(),
            working_directory.as_deref(),
            is_other_user_access_allowed,
        ),
        RouterRequestKind::AttachLookup { session_selector } => {
            lookup_session_attachment(runtime_directory, registry, &session_selector)
        }
        RouterRequestKind::ListSessions => list_session_overviews(runtime_directory, registry),
        RouterRequestKind::Restart => check_restart_binary(executable_path),
        RouterRequestKind::GrantToken {
            identity,
            scope,
            expires_in,
        } => grant_token(token_store, remote_state, identity, scope, expires_in),
        RouterRequestKind::RevokeToken { identity, scope } => {
            revoke_token(token_store, remote_state, &identity, scope.as_ref())
        }
        RouterRequestKind::ListTokens { scope } => list_token_entries(token_store, scope.as_ref()),
        RouterRequestKind::RemoteStatus => build_remote_status_result(remote_state),
        RouterRequestKind::EnableRemote => enable_remote_access(remote_state, router_events_sender),
    }
}

/// Answer one question from a remote connection the listener is holding.
///
/// The dispatcher answers one at a time, so the token store keeps its one
/// writer and the list of carried connections has a single owner.
fn serve_remote_admission(
    runtime_directory: &Path,
    token_store: Option<&Path>,
    registry: &SessionRegistry,
    remote_state: &mut RemoteState,
    admission_question: AdmissionAsk,
) {
    match admission_question {
        AdmissionAsk::Admit {
            connection_token,
            remote_connection_stream,
            response_sender,
        } => {
            let _ = response_sender.send(admit_remote_token(
                token_store,
                remote_state,
                &connection_token,
                remote_connection_stream,
            ));
        }
        AdmissionAsk::Rows {
            scope,
            response_sender,
        } => {
            let _ = response_sender.send(list_remote_session_rows(registry, &scope));
        }
        AdmissionAsk::Locate {
            scope,
            remote_connection_id,
            session_selector,
            response_sender,
        } => {
            let _ = response_sender.send(locate_remote_session(
                runtime_directory,
                registry,
                remote_state,
                &scope,
                remote_connection_id,
                &session_selector,
            ));
        }
        AdmissionAsk::Ended {
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
/// A list already holding [`MAX_LIVE_REMOTE_CONNECTION_COUNT`] admits nothing more. That count
/// is read before the secret is, so a caller arriving at a full list does the
/// same work as one presenting a wrong secret.
///
/// The store is written back, stamping that record's last-used time. A store
/// that cannot be read or written admits nothing.
fn admit_remote_token(
    token_store: Option<&Path>,
    remote_state: &mut RemoteState,
    connection_token: &ConnectionToken,
    remote_connection_stream: TcpStream,
) -> Option<Admitted> {
    if remote_state.admitted_remote_connections.len() >= MAX_LIVE_REMOTE_CONNECTION_COUNT {
        if remote_state.full_capacity_warning.is_due(Instant::now()) {
            tracing::warn!(
                "{MAX_LIVE_REMOTE_CONNECTION_COUNT} remote connections are already admitted; \
                 refusing the ones that arrive until some of them end"
            );
        }
        return None;
    }
    let Ok((token_store_path, mut token_store)) = open_token_store(token_store) else {
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
    Some(Admitted {
        scope,
        remote_connection_id,
    })
}

/// Whether this router started the session `entry` describes.
///
/// A session another local user started carries `pid` `0`; a session this
/// router started carries the process id of its session server, which is never
/// `0`.
///
/// [`list_remote_session_rows`] and [`locate_remote_session`] both read this, so a remote caller is
/// shown exactly the sessions it can be carried to.
fn is_session_started_by_this_router(session_entry: &SessionRecord) -> bool {
    session_entry.process_id != 0
}

/// The sessions an admitted scope reaches, in name then id order.
///
/// A host-wide scope reaches every session this router started; a session scope
/// reaches that one session. A session another local user started is left out,
/// on the rule [`is_session_started_by_this_router`] states. Nothing outside
/// the router's
/// own list is read.
fn list_remote_session_rows(
    registry: &SessionRegistry,
    scope: &TokenScope,
) -> Vec<RemoteSessionRow> {
    let mut remote_session_rows: Vec<RemoteSessionRow> = registry
        .iter()
        .filter(|(session_id, session_entry)| {
            scope.is_allowed_for_session(**session_id)
                && is_session_started_by_this_router(session_entry)
        })
        .map(|(session_id, session_entry)| RemoteSessionRow {
            session_id: *session_id,
            session_name: session_entry.session_name.clone(),
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
/// connection numbered `remote_connection_id` still stands, its scope covers that session, and
/// this router started it.
///
/// Checks in this order, reading no caller-supplied name until the last step:
/// the connection numbered `remote_connection_id` is still registered, `session_selector` names a session
/// in the router's own in-memory list, `scope` covers that session, and
/// [`is_session_started_by_this_router`] holds for it. No socket is opened, nothing is
/// waited for, and no file is touched.
///
/// `None` for all four failures: a connection a revoke dropped, a session selector
/// naming no session, a session the scope does not cover, and a session another
/// local user started.
fn locate_remote_session(
    runtime_directory: &Path,
    registry: &SessionRegistry,
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
    let session_id = resolve_session_selector(registry, session_selector)?;
    if !scope.is_allowed_for_session(session_id) {
        return None;
    }
    if !is_session_started_by_this_router(&registry[&session_id]) {
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
/// `enabled` and `listening` are separate answers: an operator who said yes on
/// a machine whose address something else holds reads `enabled: true` and
/// `listening: false`.
fn build_remote_status_result(remote_state: &RemoteState) -> RouterResult {
    let data_directory = remote_state.data_directory.as_deref();
    RouterResult::RemoteStatus {
        remote_listen_address: remote_state.remote_listen_address.clone(),
        is_remote_access_enabled: data_directory.is_some_and(is_remote_enabled),
        is_listening: remote_state.listening,
        certificate_fingerprint: data_directory
            .and_then(|data_directory| {
                CertFile::load_from_path(&CertFile::resolve_certificate_file_path(data_directory))
                    .ok()
            })
            .map(|certificate_file| {
                tls::compute_certificate_fingerprint(&certificate_file.cert_der)
            }),
        remote_connection_count: Some(remote_state.admitted_remote_connections.len()),
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
    router_events_sender: &Sender<RouterEvent>,
) -> RouterResult {
    let Some(remote_listen_address) = remote_state.remote_listen_address.clone() else {
        return build_refused_result(
            "no remote listen address is set; add `remote-listen \"<host:port>\"` to koshi.kdl"
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
            Ok(certificate) => certificate,
            Err(certificate_error) => return build_refused_result(certificate_error.to_string()),
        };

    let bound_listener = if remote_state.listening {
        None
    } else {
        match remote_listener::bind_remote_listener(
            remote_listen_address.clone(),
            &certificate_file,
        ) {
            Ok(bound_listener) => Some(bound_listener),
            Err(bind_error) => {
                return build_refused_result(format!(
                    "the remote listener could not open {remote_listen_address}: {bind_error}"
                ))
            }
        }
    };

    let enabled_file = EnabledFile {
        file_format: ENABLED_FILE_FORMAT,
        enabled_at: SystemTime::now(),
    };
    if let Err(enabled_file_write_error) =
        enabled_file.write_to_path(&EnabledFile::resolve_enabled_file_path(&data_directory))
    {
        // Dropping the bound port gives it back.
        drop(bound_listener);
        return build_refused_result(enabled_file_write_error.to_string());
    }

    if let Some(bound_listener) = bound_listener {
        bound_listener.start_serving(router_events_sender.clone());
        remote_state.listening = true;
    }
    RouterResult::RemoteEnabled {
        remote_listen_address,
        certificate_fingerprint,
    }
}

/// The remote access token store at `token_store`, with the path to write it
/// back to.
///
/// `None` means this machine has no data directory to hold a store. A store
/// whose bytes cannot be read is refused, so a malformed file refuses every
/// token request and changes nothing.
fn open_token_store(token_store: Option<&Path>) -> Result<(&Path, TokenStore), RouterResult> {
    let Some(token_store_path) = token_store else {
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
/// checked add: a span the clock cannot represent is refused before anything
/// is written, so the store file is left as it stood.
///
/// A grant takes the place of whatever `identity` held on `scope`, so every
/// connection the replaced secret admitted is ended once the new record is
/// written. The hashes are taken before the replace, since the records holding
/// them are gone after it.
fn grant_token(
    token_store: Option<&Path>,
    remote_state: &mut RemoteState,
    identity: String,
    scope: TokenScope,
    expires_in: Option<Duration>,
) -> RouterResult {
    let (token_store_path, mut token_store) = match open_token_store(token_store) {
        Ok(opened) => opened,
        Err(refusal) => return refusal,
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
                    .is_none_or(|expiry| expiry > issued_at)
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
        did_replace_active_grant: has_replaced_active_grant,
    }
}

/// Stop the grants `identity` holds, narrowed to one scope when `scope` is
/// given, and write the store back when this call stopped anything.
///
/// Every connection those grants admitted ends once the store is written, so a
/// revoke ends the connection rather than refusing its next command. A
/// connection that never attached ends with the rest. The hashes are taken
/// before the revoke, since the records carry their stopped time afterwards.
fn revoke_token(
    token_store: Option<&Path>,
    remote_state: &mut RemoteState,
    identity: &str,
    scope: Option<&TokenScope>,
) -> RouterResult {
    let (token_store_path, mut token_store) = match open_token_store(token_store) {
        Ok(opened) => opened,
        Err(refusal) => return refusal,
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
fn list_token_entries(token_store: Option<&Path>, scope: Option<&TokenScope>) -> RouterResult {
    match open_token_store(token_store) {
        Ok((_, token_store)) => RouterResult::Tokens(token_store.list_token_entries(scope)),
        Err(refusal) => refusal,
    }
}

/// Answer a restart request by checking the binary at `exe`. A binary that
/// cannot be read is refused; on Unix, one with no execute permission is
/// refused too. Nothing is torn down either way.
fn check_restart_binary(executable_path: &Path) -> RouterResult {
    match is_binary_runnable(executable_path) {
        Ok(()) => RouterResult::Restarting,
        Err(message) => build_refused_result(message),
    }
}

/// Start a session server and register the session it reports. `working_directory` is the
/// directory the new session's first shell opens in.
///
/// The child is started first and answered only once it reports the address
/// it bound. A start that fails, reports nothing within [`SESSION_SERVER_READY_TIMEOUT_DURATION`],
/// reports something unreadable, or speaks another control-plane protocol
/// version ends with the child killed, nothing registered, and whatever it
/// advertised removed. A `working_directory` the child cannot enter fails the start.
///
/// `is_other_user_access_allowed` `Some(true)` starts the session server under
/// [`ALLOW_OTHER_USERS_FLAG`], so the session serves the other users of this
/// machine whatever its `koshi.kdl` says.
fn create_session(
    runtime_directory: &Path,
    registry: &mut SessionRegistry,
    router_events_sender: &Sender<RouterEvent>,
    profile: Option<&str>,
    working_directory: Option<&Path>,
    is_other_user_access_allowed: Option<bool>,
) -> RouterResult {
    let session_id = SessionId::new();
    let session_name = generate_name(NameKind::Session, |candidate_session_name| {
        is_session_name_taken(registry, candidate_session_name)
    });

    let session_server_process = build_session_server_command(
        runtime_directory,
        session_id,
        &session_name,
        profile,
        working_directory,
        is_other_user_access_allowed,
    )
    .and_then(|mut session_server_command| session_server_command.spawn());
    let mut child_process = match session_server_process {
        Ok(child_process) => child_process,
        Err(session_start_error) => {
            return build_refused_result(format!(
                "the session could not be started: {session_start_error}"
            ))
        }
    };
    let process_id = child_process.id();

    let Some(stdout) = child_process.stdout.take() else {
        terminate_child_process(&mut child_process);
        return build_refused_result(
            "the session server started without a readable output".to_string(),
        );
    };
    let (ready_report_sender, ready_report_receiver) = mpsc::channel();
    let ready_report_reader_thread = std::thread::Builder::new()
        .name("koshi-router-ready".to_string())
        .spawn(move || {
            let _ = ready_report_sender.send(read_session_server_ready_line(stdout));
        });
    if let Err(ready_report_reader_error) = ready_report_reader_thread {
        terminate_child_process(&mut child_process);
        return build_refused_result(format!(
            "the session could not be watched for startup: {ready_report_reader_error}"
        ));
    }

    // The dispatcher waits for this startup report before serving another
    // event.
    let ready_report = match validate_session_server_ready(
        ready_report_receiver
            .recv_timeout(SESSION_SERVER_READY_TIMEOUT_DURATION)
            .ok()
            .flatten(),
    ) {
        Ok(ready_report) => ready_report,
        Err(reason) => {
            terminate_child_process(&mut child_process);
            // A child that bound its socket before it was killed left an
            // endpoint file behind; this takes it back off the disk.
            remove_session_from_registry(runtime_directory, registry, session_id);
            return build_refused_result(reason);
        }
    };

    registry.insert(
        session_id,
        SessionRecord {
            session_name: session_name.clone(),
            socket_address: ready_report.socket_address.clone(),
            process_id,
        },
    );
    start_session_reaper_thread(child_process, session_id, router_events_sender.clone());

    RouterResult::Created(SessionAddress {
        session_id,
        session_name,
        socket_address: ready_report.socket_address,
        process_id,
    })
}

/// Look one session up and hand back where it listens.
///
/// The address is probed before it is handed out. A probe that finds nothing
/// listening means the session server is gone: its entry and the files it left
/// behind are removed, and the answer is the [`build_session_not_found_result`] refusal a selector
/// naming no session gets.
fn lookup_session_attachment(
    runtime_directory: &Path,
    registry: &mut SessionRegistry,
    session_selector: &SessionSelector,
) -> RouterResult {
    let Some(session_id) = resolve_session_selector(registry, session_selector) else {
        return build_session_not_found_result(session_selector);
    };
    let socket_address = registry[&session_id].socket_address.clone();
    match Connection::connect(&socket_address) {
        // The probe sends nothing; the session server's serving thread reads
        // end of stream and returns.
        Ok(probe) => {
            drop(probe);
            let session_entry = &registry[&session_id];
            RouterResult::Found(SessionAddress {
                session_id,
                session_name: session_entry.session_name.clone(),
                socket_address,
                process_id: session_entry.process_id,
            })
        }
        Err(IpcError::NoListener { .. }) => {
            remove_session_from_registry(runtime_directory, registry, session_id);
            build_session_not_found_result(session_selector)
        }
        Err(ipc_error) => {
            build_refused_result(format!("the session could not be reached: {ipc_error}"))
        }
    }
}

/// Whether a failed description says the session is gone.
///
/// [`CliError::SessionNotFound`] is the only failure that means nothing is
/// listening on the session's socket. Every other failure —
/// [`CliError::IpcUnavailable`] for a settled protocol version outside this
/// build's range, a refusal, or an endpoint file this build cannot read — comes
/// from a session that is still bound and serving.
fn describes_a_session_that_is_gone(cli_error: &CliError) -> bool {
    matches!(cli_error, CliError::SessionNotFound { .. })
}

/// Describe every running session, in name then id order.
///
/// Each entry is asked to describe itself. An entry that nothing listens for is
/// removed by [`remove_session_from_registry`] and left out of the answer. An entry that is
/// listening but could not answer keeps its files and its place in the list,
/// and is left out of this answer only.
fn list_session_overviews(
    runtime_directory: &Path,
    registry: &mut SessionRegistry,
) -> RouterResult {
    let mut session_discoveries = Vec::new();
    let mut gone_session_ids = Vec::new();
    for session_id in registry.keys().copied() {
        match ipc_client::fetch_session_overview(runtime_directory, session_id) {
            Ok(overview) => session_discoveries.push(overview.session),
            Err(cli_error) if describes_a_session_that_is_gone(&cli_error) => {
                gone_session_ids.push(session_id)
            }
            Err(_) => {}
        }
    }
    for session_id in gone_session_ids {
        remove_session_from_registry(runtime_directory, registry, session_id);
    }
    session_discoveries.sort_by(|left_session, right_session| {
        left_session
            .session_name
            .cmp(&right_session.session_name)
            .then(left_session.session_id.cmp(&right_session.session_id))
    });
    RouterResult::Sessions(session_discoveries)
}

/// Rebuild the session list from what is already running.
///
/// Every advertised session is read for its address and process id and asked
/// to describe itself. One that answers both is registered — a session server
/// that outlived an earlier router is picked up here. One whose endpoint file
/// cannot be read, and one that nothing listens for, is removed by
/// [`remove_session_from_registry`]: its endpoint file, its resume file, and on Unix its socket
/// file go. One that is listening but could not answer keeps its files and is
/// left out of the list.
///
/// The walk is over endpoint files, which exist on every platform, so a
/// Windows pipe with no directory entry of its own is still found.
///
/// `shared_sessions_base_directory` is the machine-wide shared directory while
/// `allow-other-users` is on, and `None` while it is off. Each session it
/// advertises for another local user is asked over the address it names, and
/// registered when it answers. One that does not answer is left out and
/// nothing of it is removed: its files belong to that user.
///
/// The last step walks `runtime_directory` for resume files, which the walk over
/// endpoint files cannot reach once the endpoint file is gone, and removes the
/// ones [`remove_orphan_resume_files`] finds no owner for.
fn rebuild_session_registry(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
) -> SessionRegistry {
    let mut registry = SessionRegistry::new();
    for session_id in ipc_client::list_advertised_sessions(runtime_directory) {
        let endpoint_file = EndpointFile::load_from_path(
            &EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id),
        );
        let overview = ipc_client::fetch_session_overview(runtime_directory, session_id);
        match (endpoint_file, overview) {
            (Ok(endpoint_file), Ok(overview)) => {
                registry.insert(
                    session_id,
                    SessionRecord {
                        session_name: overview.session.session_name,
                        socket_address: endpoint_file.socket_address,
                        process_id: endpoint_file.process_id,
                    },
                );
            }
            (Ok(_), Err(session_overview_error))
                if !describes_a_session_that_is_gone(&session_overview_error) => {}
            _ => remove_session_from_registry(runtime_directory, &mut registry, session_id),
        }
    }
    for (session_id, foreign_socket_address) in
        shared_sessions_base_directory
            .into_iter()
            .flat_map(|shared_base_directory| {
                ipc_client::list_foreign_sessions(shared_base_directory, runtime_directory)
            })
    {
        if let Ok(overview) =
            ipc_client::fetch_foreign_session_overview(session_id, &foreign_socket_address)
        {
            registry.insert(
                session_id,
                SessionRecord {
                    session_name: overview.session.session_name,
                    socket_address: foreign_socket_address,
                    process_id: 0,
                },
            );
        }
    }
    remove_orphan_resume_files(runtime_directory, &registry);
    registry
}

/// True when a session in the list already carries `candidate` as its name.
fn is_session_name_taken(registry: &SessionRegistry, candidate_session_name: &str) -> bool {
    registry
        .values()
        .any(|session_entry| session_entry.session_name == candidate_session_name)
}

/// The id a selector names, or `None` when the list holds no such session.
/// A name matches only in full.
fn resolve_session_selector(
    registry: &SessionRegistry,
    session_selector: &SessionSelector,
) -> Option<SessionId> {
    match session_selector {
        SessionSelector::SessionId(session_id) => {
            registry.contains_key(session_id).then_some(*session_id)
        }
        SessionSelector::SessionName(session_name) => registry
            .iter()
            .find(|(_, session_entry)| session_entry.session_name == *session_name)
            .map(|(session_id, _)| *session_id),
    }
}

/// Drop one session from the list and remove every file it left in
/// `runtime_directory`: its endpoint file, its resume file, and on Unix its socket
/// file. All three are derived from the id, so this works for an entry that was
/// never in the list. A session another local user started left none of them
/// here, so nothing of that user's is removed.
///
/// A session that is replacing its own process image is left alone. Its socket
/// is unbound for that moment, which every way the router notices a dead
/// session also notices; its new image rebinds the socket and rewrites the
/// endpoint file. Past that window the swap is dead, so its resume file goes
/// with the rest.
fn remove_session_from_registry(
    runtime_directory: &Path,
    registry: &mut SessionRegistry,
    session_id: SessionId,
) {
    if crate::session_server::is_replacing_its_image(runtime_directory, session_id) {
        return;
    }
    registry.remove(&session_id);
    let _ = std::fs::remove_file(EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ));
    let _ = std::fs::remove_file(resolve_resume_file_path(runtime_directory, session_id));
    remove_socket_file(&compute_socket_address(runtime_directory, session_id));
}

/// Remove every resume file in `runtime_directory` that no session in `registry`
/// claims and that is older than
/// [`RESTART_WINDOW_DURATION`](koshi_ipc::endpoint::RESTART_WINDOW_DURATION).
///
/// A swap that never reached its new image leaves the file behind with no
/// endpoint file beside it, so the walk over endpoint files never sees it. That
/// happens when the new image is killed before it reads the file, and when the
/// machine loses power mid-swap. A new image that starts at all removes the
/// file on every way out, so a swap that got that far leaves no orphan.
///
/// A file younger than the window belongs to a swap that is still in flight, and
/// a file whose session is in the list belongs to a session that is running, so
/// neither is touched.
fn remove_orphan_resume_files(runtime_directory: &Path, registry: &SessionRegistry) {
    for session_id in ipc_client::list_sessions_with_resume_files(runtime_directory) {
        if registry.contains_key(&session_id)
            || crate::session_server::is_replacing_its_image(runtime_directory, session_id)
        {
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
/// On Windows the server runs with the `CREATE_NO_WINDOW` creation flag, so
/// its console carries no window on screen.
fn build_session_server_command(
    runtime_directory: &Path,
    session_id: SessionId,
    session_name: &str,
    profile: Option<&str>,
    working_directory: Option<&Path>,
    is_other_user_access_allowed: Option<bool>,
) -> std::io::Result<std::process::Command> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg(SESSION_SERVER_SUBCOMMAND)
        .arg(session_id.to_string())
        .arg(session_name)
        .arg(RUNTIME_DIRECTORY_FLAG)
        .arg(runtime_directory);
    if let Some(profile) = profile {
        command.arg(PROFILE_FLAG).arg(profile);
    }
    if is_other_user_access_allowed == Some(true) {
        command.arg(ALLOW_OTHER_USERS_FLAG);
    }
    if let Some(working_directory) = working_directory {
        command.current_dir(working_directory);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(command)
}

/// The line a freshly spawned session server printed, or the refusal to answer
/// with.
///
/// `None` is a session server that printed nothing readable before the wait
/// ran out. A report naming another control-plane protocol version comes from
/// a koshi binary that is a different build from this running router: the
/// router spawns the binary now on disk, and that binary can be replaced while
/// the router keeps serving.
fn validate_session_server_ready(
    ready_report: Option<SessionServerReady>,
) -> Result<SessionServerReady, String> {
    let Some(ready_report) = ready_report else {
        return Err("the session did not report a bound socket".to_string());
    };
    if ready_report.protocol_version != ROUTER_PROTOCOL_VERSION {
        return Err(format!(
            "the koshi binary on disk speaks control-plane protocol version {} and this running \
             router speaks {ROUTER_PROTOCOL_VERSION}, so they are different builds; the router \
             serves its own build until it restarts, which it does once no session is left \
             running",
            ready_report.protocol_version
        ));
    }
    Ok(ready_report)
}

/// Watch one session server until it exits, then report the exit so its
/// session leaves the list.
///
/// A thread that cannot be started leaves the session in the list unwatched;
/// the next lookup or listing probes its socket and removes it there.
fn start_session_reaper_thread(
    mut child_process: Child,
    session_id: SessionId,
    router_events_sender: Sender<RouterEvent>,
) {
    let _ = std::thread::Builder::new()
        .name("koshi-router-child".to_string())
        .spawn(move || {
            let _ = child_process.wait();
            let _ = router_events_sender.send(RouterEvent::ChildExited(session_id));
        });
}

/// Watch one session the rebuild picked up until it exits, then report the
/// exit so its session leaves the list.
///
/// After a restart in place, the sessions the previous image started are still
/// children of this process, and this thread reports their exits. A session
/// this process is not the parent of fails the wait with `ECHILD` and ends the
/// thread; the next lookup or listing probes its socket and removes it there.
#[cfg(unix)]
fn watch_session_process_exit(
    process_id: u32,
    session_id: SessionId,
    router_events_sender: Sender<RouterEvent>,
) {
    let _ = std::thread::Builder::new()
        .name("koshi-router-child".to_string())
        .spawn(move || {
            let mut process_wait_status = 0;
            if unsafe { libc::waitpid(process_id as libc::pid_t, &mut process_wait_status, 0) }
                != -1
            {
                let _ = router_events_sender.send(RouterEvent::ChildExited(session_id));
            }
        });
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

/// End a child that never became a session, and collect it so no process is
/// left behind.
fn terminate_child_process(child_process: &mut Child) {
    let _ = child_process.kill();
    let _ = child_process.wait();
}

/// A refusal carrying `message`, under [`IpcErrorCode::MalformedRequest`].
///
/// Every refusal the router answers takes this code except a session it does
/// not have, which goes through [`build_session_not_found_result`].
fn build_refused_result(message: String) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::MalformedRequest,
        message,
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
