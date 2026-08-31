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
use koshi_ipc::endpoint::{remove_socket_file, resume_path, socket_addr, EndpointFile};
use koshi_ipc::error::{IpcError, RemoteFile};
use koshi_ipc::plane::{self, Next};
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::remote_state::{
    remote_enabled, CertFile, EnabledFile, CERT_FILE_FORMAT, ENABLED_FILE_FORMAT,
};
use koshi_ipc::remote_tokens::{hash_token, store_path, TokenScope, TokenStore};
use koshi_ipc::remote_wire::RemoteSessionRow;
use koshi_ipc::router::{
    router_endpoint_path, router_lock_path, router_socket_addr, ControlPlane, RouterHandshake,
    RouterRequestKind, RouterResponse, RouterResult, SessionAddress, SessionSelector,
    SessionServerReady, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::tls;
use koshi_ipc::transport::{self, Connection, Listener};
use koshi_ipc::validate::{reclaim_stale_socket, validate_socket_addr};

use koshi_link::error::CliError;
use koshi_link::ipc_client;
use koshi_link::router_client::{ROUTER_SUBCOMMAND, RUNTIME_DIR_FLAG};
use koshi_runtime::server::binary_is_runnable;

use crate::process;
use crate::remote_listener::{self, AdmissionAsk, Admitted, Occasional};
use crate::session_server::{ALLOW_OTHER_USERS_FLAG, SESSION_SERVER_SUBCOMMAND};

#[cfg(test)]
mod tests;

/// The version of the binary this router is, reported in its Hello answer.
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the dispatcher waits for a request while no session is running.
/// A window that passes with the list still empty ends the router.
const ROUTER_IDLE_EXIT: Duration = Duration::from_secs(30);

/// How long a newly started session server has to report the address it
/// bound. A slower start is treated as a failed start.
const READY_WAIT: Duration = Duration::from_secs(10);

/// How long the accept loop pauses after a failed accept before trying
/// again, so a persistent accept error cannot spin a core.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// How long a replacement router waits for the previous router to release the
/// router lock. The operating system releases that lock if the previous router
/// dies.
const LOCK_HANDOVER_WAIT: Duration = Duration::from_secs(10);

/// How long the lock wait pauses between attempts on the router lock.
const LOCK_HANDOVER_POLL: Duration = Duration::from_millis(100);

/// How long shutdown pauses after withdrawing the socket, giving a serving
/// thread time to finish the reply it is writing.
const DRAIN_GRACE: Duration = Duration::from_millis(100);

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
type Registry = HashMap<SessionId, SessionEntry>;

/// What the router knows about one running session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionEntry {
    /// The session's generated display name.
    name: String,
    /// The session's control-socket address: a socket-file path on Unix, a
    /// bare pipe name on Windows.
    socket: String,
    /// The process id of the session server serving that socket, and `0` for
    /// a session another local user started, whose process this router does
    /// not own.
    pid: u32,
}

/// One thing for the dispatcher to do.
pub(crate) enum RouterEvent {
    /// A request read off a connection, with the channel its answer goes back
    /// on.
    Request {
        /// What is being asked.
        kind: RouterRequestKind,
        /// Where the answer goes.
        reply: Sender<RouterResult>,
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
struct LiveRemote {
    /// The sha256 of the secret that admitted this connection.
    hash: String,
    /// The connection's socket, held so a revoke can end it.
    stream: TcpStream,
    /// The number this connection is registered under, which the listener
    /// reports when the connection ends.
    id: u64,
}

/// How many remote connections this machine holds admitted at once.
///
/// Each one keeps a socket handle in [`RemoteState::live`] and a thread in the
/// listener. A connection arriving over this count is refused in the sentence
/// every refusal carries, and nothing is registered for it.
pub(crate) const MAX_LIVE_REMOTE: usize = 128;

/// What the router holds for remote clients: where the listener binds, whether
/// it is open, and the connections it has admitted.
///
/// Owned by the dispatcher loop alone, as the session list is.
struct RemoteState {
    /// The address `koshi.kdl` names, or `None` when it names none.
    address: Option<String>,
    /// The koshi data directory holding the certificate and the record of the
    /// operator's yes, or `None` when this machine has none.
    data_dir: Option<PathBuf>,
    /// Whether the listener is open.
    listening: bool,
    /// The remote connections this machine has admitted, whether they have
    /// attached to a session or not. Never longer than [`MAX_LIVE_REMOTE`].
    live: Vec<LiveRemote>,
    /// The number the next admitted connection is registered under.
    next_id: u64,
    /// The warning written when the list is full.
    said_full: Occasional,
}

impl RemoteState {
    /// End every admitted connection a secret in `hashes` opened, and drop it
    /// from the list.
    ///
    /// Each connection's socket is shut down in both directions, ending the
    /// thread reading it and its two bridge threads when it has attached. A
    /// later attach on a dropped record is refused.
    ///
    /// Called on a revoke and on a grant that replaces a standing one. An
    /// expiry calls nothing.
    fn cut(&mut self, hashes: &[String]) {
        self.live.retain(|live| {
            if !hashes.contains(&live.hash) {
                return true;
            }
            let _ = live.stream.shutdown(Shutdown::Both);
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
/// to that router instead. `wait_for_lock` waits up to `LOCK_HANDOVER_WAIT`
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
    runtime_dir: &Path,
    wait_for_lock: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    koshi_paths::ensure_private_dir(runtime_dir)?;

    // The path this program was started from, read once here. A restart runs
    // the binary at this path.
    let exe = std::env::current_exe()?;

    // Where this machine's remote access tokens live, resolved once here. A
    // machine with no resolvable data directory has no store, so it holds no
    // remote access token.
    let data_dir = koshi_paths::data_dir();
    let token_store = data_dir.as_deref().map(store_path);

    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(router_lock_path(runtime_dir))?;
    if !take_lock(&lock_file, wait_for_lock)? {
        return Ok(());
    }

    // Trust order: the address is checked against the private directory
    // before anything at it is touched.
    let addr = router_socket_addr(runtime_dir);
    validate_socket_addr(&addr, runtime_dir)?;
    reclaim_stale_socket(&addr)?;
    let listener = Listener::bind(&addr)?;

    let token = ConnectionToken::generate();
    let endpoint_path = router_endpoint_path(runtime_dir);
    let endpoint = EndpointFile {
        socket: addr.clone(),
        token: token.clone(),
        pid: std::process::id(),
    };
    if let Err(error) = endpoint.write(&endpoint_path) {
        drop(listener);
        remove_socket_file(&addr);
        return Err(error.into());
    }

    let mut registry = sweep(runtime_dir, ipc_client::shared_base().as_deref());

    let (events_tx, events_rx) = mpsc::channel();
    let shutting_down = Arc::new(AtomicBool::new(false));
    let accept_thread =
        match start_accept_thread(listener, token, events_tx.clone(), &shutting_down) {
            Ok(handle) => handle,
            Err(error) => {
                let _ = std::fs::remove_file(&endpoint_path);
                remove_socket_file(&addr);
                return Err(error.into());
            }
        };

    let mut remote = RemoteState {
        address: merge_server(
            ServerConfig::default(),
            koshi_link::config::load_app_layer().into_iter().collect(),
        )
        .remote_listen,
        data_dir,
        listening: false,
        live: Vec::new(),
        next_id: 0,
        said_full: Occasional::new(),
    };
    open_remote_listener(&mut remote, &events_tx);

    #[cfg(unix)]
    for (id, entry) in &registry {
        if entry.pid != 0 {
            watch_session_exit(entry.pid, *id, events_tx.clone());
        }
    }

    loop {
        match dispatch(
            runtime_dir,
            &exe,
            token_store.as_deref(),
            &events_tx,
            &events_rx,
            ROUTER_IDLE_EXIT,
            &mut registry,
            &mut remote,
        ) {
            RouterExit::Idle => break,
            RouterExit::Restart => {
                #[cfg(unix)]
                {
                    // The call returns only when the exec failed, having put
                    // the SIGPIPE ignore back; the loop serves on.
                    let _ = restart_by_exec(&exe, runtime_dir);
                }
                #[cfg(windows)]
                // The new router waits for the lock this one drops last; a
                // spawn that failed leaves this router serving.
                if hand_over_to(&exe, runtime_dir).is_ok() {
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
    if let Ok(wake) = Connection::connect(&addr) {
        let _ = accept_thread.join();
        drop(wake);
    }
    let _ = std::fs::remove_file(&endpoint_path);
    remove_socket_file(&addr);
    // ponytail: a serving thread blocked on its peer cannot be joined, so
    // shutdown waits a fixed moment instead; a caller that loses its last
    // reply retries, which is the same path a router that has already exited
    // puts it on.
    std::thread::sleep(DRAIN_GRACE);

    drop(lock_file);
    Ok(())
}

/// Take the router lock. `true` means this process holds it, and `false` that
/// another router does.
///
/// Without `wait_for_lock` one attempt decides it. With `wait_for_lock` the
/// attempt is repeated every [`LOCK_HANDOVER_POLL`] for up to
/// [`LOCK_HANDOVER_WAIT`], and a wait that runs out reads as another router
/// holding it.
fn take_lock(lock_file: &File, wait_for_lock: bool) -> std::io::Result<bool> {
    let deadline = Instant::now() + LOCK_HANDOVER_WAIT;
    loop {
        match FileExt::try_lock(lock_file) {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) => {
                if !wait_for_lock || Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(LOCK_HANDOVER_POLL);
            }
            Err(TryLockError::Error(error)) => return Err(error),
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
fn open_remote_listener(remote: &mut RemoteState, events_tx: &Sender<RouterEvent>) {
    let Some(address) = remote.address.clone() else {
        return;
    };
    let Some(data_dir) = remote.data_dir.clone().filter(|dir| remote_enabled(dir)) else {
        tracing::info!(
            "remote listen address {address} is set, but remote access was never switched on; \
             run `koshi share grant` to switch it on"
        );
        return;
    };
    let cert = match load_or_make_cert(&data_dir) {
        Ok((cert, _)) => cert,
        Err(error) => {
            tracing::warn!(
                "the remote listener could not open {address}: {error}; local clients are \
                 unaffected"
            );
            return;
        }
    };
    let bound = match remote_listener::bind(address.clone(), &cert) {
        Ok(bound) => bound,
        Err(error) => {
            tracing::warn!(
                "the remote listener could not open {address}: {error}; local clients are \
                 unaffected"
            );
            return;
        }
    };
    bound.serve(events_tx.clone());
    remote.listening = true;
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
fn load_or_make_cert(data_dir: &Path) -> Result<(CertFile, String), IpcError> {
    let path = CertFile::path(data_dir);
    if let Ok(file) = CertFile::read(&path) {
        let fingerprint = tls::fingerprint(&file.cert_der);
        return Ok((file, fingerprint));
    }
    let made = rcgen::generate_simple_self_signed(vec!["koshi".to_string()]).map_err(|error| {
        IpcError::RemoteFileWrite {
            file: RemoteFile::Certificate,
            path: path.display().to_string(),
            detail: format!("the certificate could not be generated: {error}"),
        }
    })?;
    let file = CertFile {
        format: CERT_FILE_FORMAT,
        cert_der: made.cert.der().to_vec(),
        key_der: made.signing_key.serialize_der(),
    };
    file.write(&path)?;
    let fingerprint = tls::fingerprint(&file.cert_der);
    Ok((file, fingerprint))
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
fn restart_by_exec(exe: &Path, runtime_dir: &Path) -> std::io::Error {
    process::exec_and_keep_ignoring_sigpipe(
        std::process::Command::new(exe)
            .arg(ROUTER_SUBCOMMAND)
            .arg(RUNTIME_DIR_FLAG)
            .arg(runtime_dir),
    )
}

/// Start the binary at `exe` as a new router over the same runtime directory,
/// waiting for the lock this router still holds.
///
/// The new router is detached with a process group of its own and no console,
/// and its input and output go nowhere. An error means nothing was started.
#[cfg(windows)]
fn hand_over_to(exe: &Path, runtime_dir: &Path) -> std::io::Result<()> {
    process::detached(
        std::process::Command::new(exe)
            .arg(ROUTER_SUBCOMMAND)
            .arg(RUNTIME_DIR_FLAG)
            .arg(runtime_dir)
            .arg(WAIT_FOR_LOCK_FLAG),
    )
    .spawn()
    .map(|_| ())
}

/// Start the thread that accepts router connections.
fn start_accept_thread(
    listener: Listener,
    token: ConnectionToken,
    events_tx: Sender<RouterEvent>,
    shutting_down: &Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    let flag = Arc::clone(shutting_down);
    std::thread::Builder::new()
        .name("koshi-router-accept".to_string())
        .spawn(move || accept_loop(&listener, &token, &events_tx, &flag))
}

/// Accept connections until the shutdown flag is set, giving each its own
/// serving thread.
///
/// Only this router's own user is served. A connection opened by another user,
/// and one whose user cannot be read, is closed without being served.
fn accept_loop(
    listener: &Listener,
    token: &ConnectionToken,
    events_tx: &Sender<RouterEvent>,
    shutting_down: &AtomicBool,
) {
    transport::accept_until_shutdown(listener, shutting_down, ACCEPT_RETRY_DELAY, |connection| {
        // The OS reports which user opened the connection, so a peer cannot
        // claim to be another one.
        if !matches!(connection.peer_is_same_user(), Ok(true)) {
            return;
        }
        let token = token.clone();
        let events_tx = events_tx.clone();
        std::thread::spawn(move || serve_connection(connection, token, &events_tx));
    });
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
fn serve_connection(
    mut connection: Connection,
    token: ConnectionToken,
    events_tx: &Sender<RouterEvent>,
) {
    #[cfg(unix)]
    process::block_sigpipe_on_this_thread();
    let mut gate = RouterHandshake::new(token);
    loop {
        let (request_id, kind) = match plane::next_request::<ControlPlane>(
            &mut connection,
            &mut gate,
            BUILD_VERSION,
            &plane::always_admitted,
        ) {
            Next::Answered => continue,
            Next::Stop => return,
            Next::Dispatch { request_id, kind } => (request_id, kind),
        };

        let Some(result) = ask_dispatcher(events_tx, kind) else {
            return;
        };
        let response = RouterResponse {
            request_id: Some(request_id),
            result,
        };
        if connection.send(&response).is_err() {
            return;
        }
        if response.result == RouterResult::Restarting {
            // A send that fails means the dispatcher is gone and the router is
            // already exiting.
            let _ = events_tx.send(RouterEvent::RestartDelivered);
        }
    }
}

/// Hand one request to the dispatcher and wait for its answer. `None` means
/// the dispatcher is gone — the router is exiting — so the caller closes its
/// connection without an answer.
fn ask_dispatcher(
    events_tx: &Sender<RouterEvent>,
    kind: RouterRequestKind,
) -> Option<RouterResult> {
    let (reply, answer) = mpsc::channel();
    if events_tx
        .send(RouterEvent::Request { kind, reply })
        .is_err()
    {
        return None;
    }
    answer.recv().ok()
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
/// `events_tx` is the loop's own sender, handed to each session's reaper
/// thread so a child's exit reaches here. `token_store` is the remote access
/// token store every token request is answered against, and `remote` is what
/// the router holds for remote clients.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    runtime_dir: &Path,
    exe: &Path,
    token_store: Option<&Path>,
    events_tx: &Sender<RouterEvent>,
    events_rx: &Receiver<RouterEvent>,
    idle_exit: Duration,
    registry: &mut Registry,
    remote: &mut RemoteState,
) -> RouterExit {
    loop {
        let received = if registry.is_empty() {
            events_rx.recv_timeout(idle_exit).ok()
        } else {
            events_rx.recv().ok()
        };
        let Some(event) = received else {
            return RouterExit::Idle;
        };
        match event {
            RouterEvent::Request { kind, reply } => {
                let _ = reply.send(serve_request(
                    runtime_dir,
                    exe,
                    token_store,
                    registry,
                    remote,
                    events_tx,
                    kind,
                ));
            }
            RouterEvent::ChildExited(id) => unregister(runtime_dir, registry, id),
            RouterEvent::RestartDelivered => return RouterExit::Restart,
            RouterEvent::Admission(ask) => {
                serve_admission(runtime_dir, token_store, registry, remote, ask);
            }
        }
    }
}

/// Answer one request against the session list.
///
/// The dispatcher answers one request at a time, so the remote access token
/// store at `token_store` has one writer.
fn serve_request(
    runtime_dir: &Path,
    exe: &Path,
    token_store: Option<&Path>,
    registry: &mut Registry,
    remote: &mut RemoteState,
    events_tx: &Sender<RouterEvent>,
    kind: RouterRequestKind,
) -> RouterResult {
    match kind {
        RouterRequestKind::Hello { .. } => {
            unreachable!("Hello is answered by the connection thread before dispatch")
        }
        RouterRequestKind::CreateSession {
            profile,
            cwd,
            allow_other_users,
        } => create_session(
            runtime_dir,
            registry,
            events_tx,
            profile.as_deref(),
            cwd.as_deref(),
            allow_other_users,
        ),
        RouterRequestKind::AttachLookup { selector } => {
            attach_lookup(runtime_dir, registry, &selector)
        }
        RouterRequestKind::ListSessions => list_sessions(runtime_dir, registry),
        RouterRequestKind::Restart => restart_check(exe),
        RouterRequestKind::GrantToken {
            identity,
            scope,
            expires_in,
        } => grant_token(token_store, remote, identity, scope, expires_in),
        RouterRequestKind::RevokeToken { identity, scope } => {
            revoke_token(token_store, remote, &identity, scope.as_ref())
        }
        RouterRequestKind::ListTokens { scope } => list_tokens(token_store, scope.as_ref()),
        RouterRequestKind::RemoteStatus => remote_status(remote),
        RouterRequestKind::EnableRemote => enable_remote(remote, events_tx),
    }
}

/// Answer one question from a remote connection the listener is holding.
///
/// The dispatcher answers one at a time, so the token store keeps its one
/// writer and the list of carried connections has a single owner.
fn serve_admission(
    runtime_dir: &Path,
    token_store: Option<&Path>,
    registry: &Registry,
    remote: &mut RemoteState,
    ask: AdmissionAsk,
) {
    match ask {
        AdmissionAsk::Admit {
            token,
            stream,
            reply,
        } => {
            let _ = reply.send(admit_token(token_store, remote, &token, stream));
        }
        AdmissionAsk::Rows { scope, reply } => {
            let _ = reply.send(remote_rows(registry, &scope));
        }
        AdmissionAsk::Locate {
            scope,
            id,
            selector,
            reply,
        } => {
            let _ = reply.send(locate_remote(
                runtime_dir,
                registry,
                remote,
                &scope,
                id,
                &selector,
            ));
        }
        AdmissionAsk::Ended { id } => remote.live.retain(|live| live.id != id),
    }
}

/// What a presented secret reaches, and the number the connection presenting
/// it is registered under.
///
/// A secret that reaches something registers the connection against that
/// secret's hash, whether or not it goes on to attach. The registration is
/// dropped when the listener reports the connection ended.
///
/// A list already holding [`MAX_LIVE_REMOTE`] admits nothing more. That count
/// is read before the secret is, so a caller arriving at a full list does the
/// same work as one presenting a wrong secret.
///
/// The store is written back, stamping that record's last-used time. A store
/// that cannot be read or written admits nothing.
fn admit_token(
    token_store: Option<&Path>,
    remote: &mut RemoteState,
    token: &ConnectionToken,
    stream: TcpStream,
) -> Option<Admitted> {
    if remote.live.len() >= MAX_LIVE_REMOTE {
        if remote.said_full.due(Instant::now()) {
            tracing::warn!(
                "{MAX_LIVE_REMOTE} remote connections are already admitted; \
                 refusing the ones that arrive until some of them end"
            );
        }
        return None;
    }
    let Ok((path, mut store)) = open_store(token_store) else {
        return None;
    };
    let scope = store.admit(token, SystemTime::now())?;
    store.write(path).ok()?;
    let id = remote.next_id;
    remote.next_id += 1;
    remote.live.push(LiveRemote {
        hash: hash_token(token),
        stream,
        id,
    });
    Some(Admitted { scope, id })
}

/// Whether this router started the session `entry` describes.
///
/// A session another local user started carries `pid` `0`; a session this
/// router started carries the process id of its session server, which is never
/// `0`.
///
/// [`remote_rows`] and [`locate_remote`] both read this, so a remote caller is
/// shown exactly the sessions it can be carried to.
fn started_by_this_router(entry: &SessionEntry) -> bool {
    entry.pid != 0
}

/// The sessions an admitted scope reaches, in name then id order.
///
/// A host-wide scope reaches every session this router started; a session scope
/// reaches that one session. A session another local user started is left out,
/// on the rule [`started_by_this_router`] states. Nothing outside the router's
/// own list is read.
fn remote_rows(registry: &Registry, scope: &TokenScope) -> Vec<RemoteSessionRow> {
    let mut rows: Vec<RemoteSessionRow> = registry
        .iter()
        .filter(|(id, entry)| scope.covers(**id) && started_by_this_router(entry))
        .map(|(id, entry)| RemoteSessionRow {
            id: *id,
            name: entry.name.clone(),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    rows
}

/// The endpoint file of the session an admitted client asked for, when the
/// connection numbered `id` still stands, its scope covers that session, and
/// this router started it.
///
/// Checks in this order, reading no caller-supplied name until the last step:
/// the connection numbered `id` is still registered, `selector` names a session
/// in the router's own in-memory list, `scope` covers that session, and
/// [`started_by_this_router`] holds for it. No socket is opened, nothing is
/// waited for, and no file is touched.
///
/// `None` for all four failures: a connection a revoke dropped, a selector
/// naming no session, a session the scope does not cover, and a session another
/// local user started.
fn locate_remote(
    runtime_dir: &Path,
    registry: &Registry,
    remote: &RemoteState,
    scope: &TokenScope,
    id: u64,
    selector: &SessionSelector,
) -> Option<PathBuf> {
    if !remote.live.iter().any(|live| live.id == id) {
        return None;
    }
    let session = resolve(registry, selector)?;
    if !scope.covers(session) {
        return None;
    }
    if !started_by_this_router(&registry[&session]) {
        return None;
    }
    Some(EndpointFile::path(runtime_dir, session))
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
fn remote_status(remote: &RemoteState) -> RouterResult {
    let dir = remote.data_dir.as_deref();
    RouterResult::RemoteStatus {
        address: remote.address.clone(),
        enabled: dir.is_some_and(remote_enabled),
        listening: remote.listening,
        fingerprint: dir
            .and_then(|dir| CertFile::read(&CertFile::path(dir)).ok())
            .map(|cert| tls::fingerprint(&cert.cert_der)),
        remote_connections: Some(remote.live.len()),
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
fn enable_remote(remote: &mut RemoteState, events_tx: &Sender<RouterEvent>) -> RouterResult {
    let Some(address) = remote.address.clone() else {
        return refused(
            "no remote listen address is set; add `remote-listen \"<host:port>\"` to koshi.kdl"
                .to_string(),
        );
    };
    let Some(data_dir) = remote.data_dir.clone() else {
        return refused(
            "this machine has no data directory, so remote access cannot be switched on"
                .to_string(),
        );
    };
    let (cert, fingerprint) = match load_or_make_cert(&data_dir) {
        Ok(made) => made,
        Err(error) => return refused(error.to_string()),
    };

    let bound = if remote.listening {
        None
    } else {
        match remote_listener::bind(address.clone(), &cert) {
            Ok(bound) => Some(bound),
            Err(error) => {
                return refused(format!(
                    "the remote listener could not open {address}: {error}"
                ))
            }
        }
    };

    let record = EnabledFile {
        format: ENABLED_FILE_FORMAT,
        enabled_at: SystemTime::now(),
    };
    if let Err(error) = record.write(&EnabledFile::path(&data_dir)) {
        // Dropping the bound port gives it back.
        drop(bound);
        return refused(error.to_string());
    }

    if let Some(bound) = bound {
        bound.serve(events_tx.clone());
        remote.listening = true;
    }
    RouterResult::RemoteEnabled {
        address,
        fingerprint,
    }
}

/// The remote access token store at `token_store`, with the path to write it
/// back to.
///
/// `None` means this machine has no data directory to hold a store. A store
/// whose bytes cannot be read is refused, so a malformed file refuses every
/// token request and changes nothing.
fn open_store(token_store: Option<&Path>) -> Result<(&Path, TokenStore), RouterResult> {
    let Some(path) = token_store else {
        return Err(refused(
            "this machine has no data directory, so no remote access token can be stored"
                .to_string(),
        ));
    };
    match TokenStore::read(path) {
        Ok(store) => Ok((path, store)),
        Err(error) => Err(refused(error.to_string())),
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
    remote: &mut RemoteState,
    identity: String,
    scope: TokenScope,
    expires_in: Option<Duration>,
) -> RouterResult {
    let (path, mut store) = match open_store(token_store) {
        Ok(opened) => opened,
        Err(refusal) => return refusal,
    };
    let issued_at = SystemTime::now();
    let expires_at = match expires_in {
        None => None,
        Some(span) => match issued_at.checked_add(span) {
            Some(at) => Some(at),
            None => {
                return refused(
                    "the expiry is further ahead than this machine's clock can represent"
                        .to_string(),
                )
            }
        },
    };
    let replacing: Vec<String> = store
        .records
        .iter()
        .filter(|record| {
            record.identity == identity
                && record.scope == scope
                && record.revoked_at.is_none()
                && record.expires_at.is_none_or(|expiry| expiry > issued_at)
        })
        .map(|record| record.hash.clone())
        .collect();
    let (token, replaced) = store.grant(identity, scope, issued_at, expires_at);
    if let Err(error) = store.write(path) {
        return refused(error.to_string());
    }
    remote.cut(&replacing);
    RouterResult::Granted { token, replaced }
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
    remote: &mut RemoteState,
    identity: &str,
    scope: Option<&TokenScope>,
) -> RouterResult {
    let (path, mut store) = match open_store(token_store) {
        Ok(opened) => opened,
        Err(refusal) => return refusal,
    };
    let stopping: Vec<String> = store
        .records
        .iter()
        .filter(|record| {
            record.identity == identity
                && record.revoked_at.is_none()
                && scope.is_none_or(|wanted| *wanted == record.scope)
        })
        .map(|record| record.hash.clone())
        .collect();
    let stopped = store.revoke(identity, scope, SystemTime::now());
    if stopped.is_empty() {
        return RouterResult::Revoked(stopped);
    }
    if let Err(error) = store.write(path) {
        return refused(error.to_string());
    }
    remote.cut(&stopping);
    RouterResult::Revoked(stopped)
}

/// Every grant this machine has made, narrowed to the grants that reach
/// `scope` when one is given. The store is not written.
fn list_tokens(token_store: Option<&Path>, scope: Option<&TokenScope>) -> RouterResult {
    match open_store(token_store) {
        Ok((_, store)) => RouterResult::Tokens(store.entries(scope)),
        Err(refusal) => refusal,
    }
}

/// Answer a restart request by checking the binary at `exe`. A binary that
/// cannot be read is refused; on Unix, one with no execute permission is
/// refused too. Nothing is torn down either way.
fn restart_check(exe: &Path) -> RouterResult {
    match binary_is_runnable(exe) {
        Ok(()) => RouterResult::Restarting,
        Err(message) => refused(message),
    }
}

/// Start a session server and register the session it reports. `cwd` is the
/// directory the new session's first shell opens in.
///
/// The child is started first and answered only once it reports the address
/// it bound. A start that fails, reports nothing within [`READY_WAIT`],
/// reports something unreadable, or speaks another control-plane protocol
/// version ends with the child killed, nothing registered, and whatever it
/// advertised removed. A `cwd` the child cannot enter fails the start.
///
/// `allow_other_users` `Some(true)` starts the session server under
/// [`ALLOW_OTHER_USERS_FLAG`], so the session serves the other users of this
/// machine whatever its `koshi.kdl` says.
fn create_session(
    runtime_dir: &Path,
    registry: &mut Registry,
    events_tx: &Sender<RouterEvent>,
    profile: Option<&str>,
    cwd: Option<&Path>,
    allow_other_users: Option<bool>,
) -> RouterResult {
    let id = SessionId::new();
    let name = generate_name(NameKind::Session, |candidate| {
        name_is_taken(registry, candidate)
    });

    let started = session_server_command(runtime_dir, id, &name, profile, cwd, allow_other_users)
        .and_then(|mut command| command.spawn());
    let mut child = match started {
        Ok(child) => child,
        Err(error) => return refused(format!("the session could not be started: {error}")),
    };
    let pid = child.id();

    let Some(stdout) = child.stdout.take() else {
        kill_child(&mut child);
        return refused("the session server started without a readable output".to_string());
    };
    let (ready_tx, ready_rx) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("koshi-router-ready".to_string())
        .spawn(move || {
            let _ = ready_tx.send(read_ready_line(stdout));
        });
    if let Err(error) = reader {
        kill_child(&mut child);
        return refused(format!(
            "the session could not be watched for startup: {error}"
        ));
    }

    // ponytail: creates serialize the dispatcher; move the wait onto the
    // monitor thread if create latency matters.
    let report = match accept_ready(ready_rx.recv_timeout(READY_WAIT).ok().flatten()) {
        Ok(report) => report,
        Err(reason) => {
            kill_child(&mut child);
            // A child that bound its socket before it was killed left an
            // endpoint file behind; this takes it back off the disk.
            unregister(runtime_dir, registry, id);
            return refused(reason);
        }
    };

    registry.insert(
        id,
        SessionEntry {
            name: name.clone(),
            socket: report.socket.clone(),
            pid,
        },
    );
    start_reaper_thread(child, id, events_tx.clone());

    RouterResult::Created(SessionAddress {
        id,
        name,
        socket: report.socket,
        pid,
    })
}

/// Look one session up and hand back where it listens.
///
/// The address is probed before it is handed out. A probe that finds nothing
/// listening means the session server is gone: its entry and the files it left
/// behind are removed, and the answer is the [`not_found`] refusal a selector
/// naming no session gets.
fn attach_lookup(
    runtime_dir: &Path,
    registry: &mut Registry,
    selector: &SessionSelector,
) -> RouterResult {
    let Some(id) = resolve(registry, selector) else {
        return not_found(selector);
    };
    let socket = registry[&id].socket.clone();
    match Connection::connect(&socket) {
        // The probe sends nothing; the session server's serving thread reads
        // end of stream and returns.
        Ok(probe) => {
            drop(probe);
            let entry = &registry[&id];
            RouterResult::Found(SessionAddress {
                id,
                name: entry.name.clone(),
                socket,
                pid: entry.pid,
            })
        }
        Err(IpcError::NoListener { .. }) => {
            unregister(runtime_dir, registry, id);
            not_found(selector)
        }
        Err(error) => refused(format!("the session could not be reached: {error}")),
    }
}

/// Whether a failed description says the session is gone.
///
/// [`CliError::SessionNotFound`] is the only failure that means nothing is
/// listening on the session's socket. Every other failure —
/// [`CliError::IpcUnavailable`] for a settled protocol version outside this
/// build's range, a refusal, or an endpoint file this build cannot read — comes
/// from a session that is still bound and serving.
fn describes_a_session_that_is_gone(error: &CliError) -> bool {
    matches!(error, CliError::SessionNotFound { .. })
}

/// Describe every running session, in name then id order.
///
/// Each entry is asked to describe itself. An entry that nothing listens for is
/// removed by [`unregister`] and left out of the answer. An entry that is
/// listening but could not answer keeps its files and its place in the list,
/// and is left out of this answer only.
fn list_sessions(runtime_dir: &Path, registry: &mut Registry) -> RouterResult {
    let mut rows = Vec::new();
    let mut gone = Vec::new();
    for id in registry.keys().copied() {
        match ipc_client::fetch_overview(runtime_dir, id) {
            Ok(overview) => rows.push(overview.session),
            Err(error) if describes_a_session_that_is_gone(&error) => gone.push(id),
            Err(_) => {}
        }
    }
    for id in gone {
        unregister(runtime_dir, registry, id);
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    RouterResult::Sessions(rows)
}

/// Rebuild the session list from what is already running.
///
/// Every advertised session is read for its address and process id and asked
/// to describe itself. One that answers both is registered — a session server
/// that outlived an earlier router is picked up here. One whose endpoint file
/// cannot be read, and one that nothing listens for, is removed by
/// [`unregister`]: its endpoint file, its resume file, and on Unix its socket
/// file go. One that is listening but could not answer keeps its files and is
/// left out of the list.
///
/// The walk is over endpoint files, which exist on every platform, so a
/// Windows pipe with no directory entry of its own is still found.
///
/// `shared_base` is the machine-wide shared directory while
/// `allow-other-users` is on, and `None` while it is off. Each session it
/// advertises for another local user is asked over the address it names, and
/// registered when it answers. One that does not answer is left out and
/// nothing of it is removed: its files belong to that user.
///
/// The last step walks `runtime_dir` for resume files, which the walk over
/// endpoint files cannot reach once the endpoint file is gone, and removes the
/// ones [`remove_orphan_resume_files`] finds no owner for.
fn sweep(runtime_dir: &Path, shared_base: Option<&Path>) -> Registry {
    let mut registry = Registry::new();
    for id in ipc_client::advertised_sessions(runtime_dir) {
        let endpoint = EndpointFile::read(&EndpointFile::path(runtime_dir, id));
        let overview = ipc_client::fetch_overview(runtime_dir, id);
        match (endpoint, overview) {
            (Ok(endpoint), Ok(overview)) => {
                registry.insert(
                    id,
                    SessionEntry {
                        name: overview.session.name,
                        socket: endpoint.socket,
                        pid: endpoint.pid,
                    },
                );
            }
            (Ok(_), Err(error)) if !describes_a_session_that_is_gone(&error) => {}
            _ => unregister(runtime_dir, &mut registry, id),
        }
    }
    for (id, socket) in shared_base
        .into_iter()
        .flat_map(|base| ipc_client::foreign_sessions(base, runtime_dir))
    {
        if let Ok(overview) = ipc_client::fetch_foreign_overview(id, &socket) {
            registry.insert(
                id,
                SessionEntry {
                    name: overview.session.name,
                    socket,
                    pid: 0,
                },
            );
        }
    }
    remove_orphan_resume_files(runtime_dir, &registry);
    registry
}

/// True when a session in the list already carries `candidate` as its name.
fn name_is_taken(registry: &Registry, candidate: &str) -> bool {
    registry.values().any(|entry| entry.name == candidate)
}

/// The id a selector names, or `None` when the list holds no such session.
/// A name matches only in full.
fn resolve(registry: &Registry, selector: &SessionSelector) -> Option<SessionId> {
    match selector {
        SessionSelector::Id(id) => registry.contains_key(id).then_some(*id),
        SessionSelector::Name(name) => registry
            .iter()
            .find(|(_, entry)| entry.name == *name)
            .map(|(id, _)| *id),
    }
}

/// Drop one session from the list and remove every file it left in
/// `runtime_dir`: its endpoint file, its resume file, and on Unix its socket
/// file. All three are derived from the id, so this works for an entry that was
/// never in the list. A session another local user started left none of them
/// here, so nothing of that user's is removed.
///
/// A session that is replacing its own process image is left alone. Its socket
/// is unbound for that moment, which every way the router notices a dead
/// session also notices; its new image rebinds the socket and rewrites the
/// endpoint file. Past that window the swap is dead, so its resume file goes
/// with the rest.
fn unregister(runtime_dir: &Path, registry: &mut Registry, id: SessionId) {
    if crate::session_server::is_replacing_its_image(runtime_dir, id) {
        return;
    }
    registry.remove(&id);
    let _ = std::fs::remove_file(EndpointFile::path(runtime_dir, id));
    let _ = std::fs::remove_file(resume_path(runtime_dir, id));
    remove_socket_file(&socket_addr(runtime_dir, id));
}

/// Remove every resume file in `runtime_dir` that no session in `registry`
/// claims and that is older than
/// [`RESTART_WINDOW`](koshi_ipc::endpoint::RESTART_WINDOW).
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
fn remove_orphan_resume_files(runtime_dir: &Path, registry: &Registry) {
    for id in ipc_client::sessions_with_resume_files(runtime_dir) {
        if registry.contains_key(&id)
            || crate::session_server::is_replacing_its_image(runtime_dir, id)
        {
            continue;
        }
        let _ = std::fs::remove_file(resume_path(runtime_dir, id));
    }
}

/// Build the command that starts one session server: its identity on the
/// command line, its output piped back for the ready report, and the
/// directory its first shell opens in.
///
/// `allow_other_users` `Some(true)` adds [`ALLOW_OTHER_USERS_FLAG`]; any other
/// value leaves the session to its own `koshi.kdl`.
///
/// On Windows the server runs with the `CREATE_NO_WINDOW` creation flag, so
/// its console carries no window on screen.
fn session_server_command(
    runtime_dir: &Path,
    id: SessionId,
    name: &str,
    profile: Option<&str>,
    cwd: Option<&Path>,
    allow_other_users: Option<bool>,
) -> std::io::Result<std::process::Command> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg(SESSION_SERVER_SUBCOMMAND)
        .arg(id.to_string())
        .arg(name)
        .arg(RUNTIME_DIR_FLAG)
        .arg(runtime_dir);
    if let Some(profile) = profile {
        command.arg(PROFILE_FLAG).arg(profile);
    }
    if allow_other_users == Some(true) {
        command.arg(ALLOW_OTHER_USERS_FLAG);
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
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
fn accept_ready(report: Option<SessionServerReady>) -> Result<SessionServerReady, String> {
    let Some(report) = report else {
        return Err("the session did not report a bound socket".to_string());
    };
    if report.protocol_version != ROUTER_PROTOCOL_VERSION {
        return Err(format!(
            "the koshi binary on disk speaks control-plane protocol version {} and this running \
             router speaks {ROUTER_PROTOCOL_VERSION}, so they are different builds; the router \
             serves its own build until it restarts, which it does once no session is left \
             running",
            report.protocol_version
        ));
    }
    Ok(report)
}

/// Watch one session server until it exits, then report the exit so its
/// session leaves the list.
///
/// A thread that cannot be started leaves the session in the list unwatched;
/// the next lookup or listing probes its socket and removes it there.
fn start_reaper_thread(mut child: Child, id: SessionId, events_tx: Sender<RouterEvent>) {
    let _ = std::thread::Builder::new()
        .name("koshi-router-child".to_string())
        .spawn(move || {
            let _ = child.wait();
            let _ = events_tx.send(RouterEvent::ChildExited(id));
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
fn watch_session_exit(pid: u32, id: SessionId, events_tx: Sender<RouterEvent>) {
    let _ = std::thread::Builder::new()
        .name("koshi-router-child".to_string())
        .spawn(move || {
            let mut status = 0;
            if unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) } != -1 {
                let _ = events_tx.send(RouterEvent::ChildExited(id));
            }
        });
}

/// Read the one ready line a session server prints. End of stream or a line
/// that is not a readable report is `None`.
fn read_ready_line(stdout: ChildStdout) -> Option<SessionServerReady> {
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// End a child that never became a session, and collect it so no process is
/// left behind.
fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// A refusal carrying `message`, under [`IpcErrorCode::MalformedRequest`].
///
/// Every refusal the router answers takes this code except a session it does
/// not have, which goes through [`not_found`].
fn refused(message: String) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::MalformedRequest,
        message,
    })
}

/// A refusal for a selector naming a session the router does not have, under
/// [`IpcErrorCode::NotFound`].
///
/// The message names the selector. A [`SessionSelector::Id`] gives
/// `no session session-<uuid> is running`, and a [`SessionSelector::Name`] of
/// `quiet-lake` gives ``no session named `quiet-lake` is running``.
fn not_found(selector: &SessionSelector) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::NotFound,
        message: match selector {
            SessionSelector::Id(id) => format!("no session {id} is running"),
            SessionSelector::Name(name) => format!("no session named `{name}` is running"),
        },
    })
}
