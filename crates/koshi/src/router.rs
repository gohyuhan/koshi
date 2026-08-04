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
//! One thread accepts connections and gives each its own serving thread; a
//! serving thread only holds a channel sender, so the session list has a
//! single owner — the dispatcher loop on the main thread. A session that dies
//! leaves the list two ways: its reaper thread reports the child's exit, or a
//! lookup finds nothing listening at its address. Both remove the entry and
//! the files it left behind.
//!
//! With no session left, the dispatcher waits one idle window for a request
//! and exits when none arrives. A caller that needs the router again starts
//! it: connect, and on failure spawn the router and retry.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use fs4::{FileExt, TryLockError};
use koshi_core::command::Command;
use koshi_core::ids::SessionId;
use koshi_core::naming::{generate_name, NameKind};
use koshi_ipc::endpoint::{remove_socket_file, socket_addr, EndpointFile};
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    router_endpoint_path, router_lock_path, router_socket_addr, RouterHandshake, RouterRequest,
    RouterRequestKind, RouterResponse, RouterResult, SessionAddress, SessionSelector,
    SessionServerReady, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::{Connection, Listener};
use koshi_ipc::validate::{reclaim_stale_socket, validate_socket_addr};

use crate::error::CliError;
use crate::ipc_client;
use crate::router_client::RUNTIME_DIR_FLAG;

#[cfg(test)]
mod tests;

/// How long the dispatcher waits for a request while no session is running.
/// A window that passes with the list still empty ends the router.
const ROUTER_IDLE_EXIT: Duration = Duration::from_secs(30);

/// How long a newly started session server has to report the address it
/// bound. A slower start is treated as a failed start.
const READY_WAIT: Duration = Duration::from_secs(10);

/// How long the accept loop pauses after a failed accept before trying
/// again, so a persistent accept error cannot spin a core.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// How long shutdown pauses after withdrawing the socket, giving a serving
/// thread time to finish the reply it is writing.
const DRAIN_GRACE: Duration = Duration::from_millis(100);

/// The subcommand the router starts itself under to run one session server.
/// The arguments after it are the session id, the session name,
/// [`RUNTIME_DIR_FLAG`] with the directory this router serves, and
/// [`PROFILE_FLAG`] when the create named a profile.
const SESSION_SERVER_SUBCOMMAND: &str = "serve-session";

/// The flag carrying a `--profile` name to the session server the router
/// starts, so the session opens that profile's tabs and panes.
const PROFILE_FLAG: &str = "--profile";

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
    /// The process id of the session server serving that socket.
    pid: u32,
    /// When the session was created.
    created_at: SystemTime,
}

/// One thing for the dispatcher to do.
enum RouterEvent {
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
}

/// Run the router until no session is left.
///
/// Takes the advisory lock first: another router already holding it means
/// this call returns `Ok(())` having bound nothing, and the caller connects
/// to that router instead. With the lock held, the socket is bound, the
/// endpoint file is written, the session list is rebuilt from what is already
/// running, and the dispatcher serves requests until an idle window passes
/// with no session running.
pub fn run_router(runtime_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    koshi_paths::ensure_private_dir(runtime_dir)?;

    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(router_lock_path(runtime_dir))?;
    match FileExt::try_lock(&lock_file) {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(error)) => return Err(error.into()),
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

    let registry = sweep(runtime_dir);

    let (events_tx, events_rx) = mpsc::channel();
    let shutting_down = Arc::new(AtomicBool::new(false));
    let accept_thread = start_accept_thread(listener, token, events_tx.clone(), &shutting_down)?;

    dispatch(
        runtime_dir.to_path_buf(),
        events_tx,
        events_rx,
        ROUTER_IDLE_EXIT,
        registry,
    );

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
/// serving thread. A failed accept pauses briefly and retries.
fn accept_loop(
    listener: &Listener,
    token: &ConnectionToken,
    events_tx: &Sender<RouterEvent>,
    shutting_down: &AtomicBool,
) {
    loop {
        let connection = listener.accept();
        if shutting_down.load(Ordering::SeqCst) {
            break;
        }
        match connection {
            Ok(connection) => {
                let token = token.clone();
                let events_tx = events_tx.clone();
                std::thread::spawn(move || serve_connection(connection, token, &events_tx));
            }
            Err(_) => std::thread::sleep(ACCEPT_RETRY_DELAY),
        }
    }
}

/// Serve one router connection until its peer hangs up or a fault closes it.
///
/// A [`RouterHandshake`] gates every request. A Hello is answered here; every
/// other kind crosses to the dispatcher and comes back as its answer. A
/// malformed-but-aligned frame is answered with
/// [`IpcErrorCode::MalformedRequest`] and the connection keeps serving.
fn serve_connection(
    mut connection: Connection,
    token: ConnectionToken,
    events_tx: &Sender<RouterEvent>,
) {
    let mut gate = RouterHandshake::new(token);
    loop {
        let request: RouterRequest = match connection.recv() {
            Ok(request) => request,
            Err(IpcError::MalformedFrame { .. }) => {
                // The frame was read whole, so the stream is still aligned;
                // only its bytes were unreadable. `request_id: None` tells the
                // caller the answer belongs to no request of its own.
                let refusal = RouterResponse {
                    request_id: None,
                    result: RouterResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::MalformedRequest,
                        message: "the bytes received are not a request this build can read"
                            .to_string(),
                    }),
                };
                if connection.send(&refusal).is_err() {
                    return;
                }
                continue;
            }
            // An oversize frame's payload was never read, so the stream's
            // framing is lost; disconnects and transport faults have no
            // stream left. All close this one connection.
            Err(_) => return,
        };

        let request_id = Some(request.request_id);
        let response = match gate.check(&request.kind) {
            Err(refusal) => RouterResponse {
                request_id,
                result: RouterResult::Error(refusal),
            },
            Ok(()) => match request.kind {
                RouterRequestKind::Hello { .. } => RouterResponse {
                    request_id,
                    result: RouterResult::Hello,
                },
                kind => match ask_dispatcher(events_tx, kind) {
                    Some(result) => RouterResponse { request_id, result },
                    None => return,
                },
            },
        };
        if connection.send(&response).is_err() {
            return;
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

/// Serve events until the router ends, and hand back the session list as it
/// stood.
///
/// While a session is running the loop blocks for the next event. While none
/// is, it waits `idle_exit` for one: an event inside that window is served
/// and the loop goes on, and a window that passes ends the router.
///
/// `events_tx` is the loop's own sender, handed to each session's reaper
/// thread so a child's exit reaches here.
fn dispatch(
    runtime_dir: PathBuf,
    events_tx: Sender<RouterEvent>,
    events_rx: Receiver<RouterEvent>,
    idle_exit: Duration,
    mut registry: Registry,
) -> Registry {
    loop {
        let received = if registry.is_empty() {
            events_rx.recv_timeout(idle_exit).map_err(|_| ())
        } else {
            events_rx.recv().map_err(|_| ())
        };
        let Ok(event) = received else {
            return registry;
        };
        match event {
            RouterEvent::Request { kind, reply } => {
                let _ = reply.send(serve_request(&runtime_dir, &mut registry, &events_tx, kind));
            }
            RouterEvent::ChildExited(id) => unregister(&runtime_dir, &mut registry, id),
        }
    }
}

/// Answer one request against the session list.
fn serve_request(
    runtime_dir: &Path,
    registry: &mut Registry,
    events_tx: &Sender<RouterEvent>,
    kind: RouterRequestKind,
) -> RouterResult {
    match kind {
        RouterRequestKind::Hello { .. } => {
            unreachable!("Hello is answered by the connection thread before dispatch")
        }
        RouterRequestKind::CreateSession { profile, cwd } => create_session(
            runtime_dir,
            registry,
            events_tx,
            profile.as_deref(),
            cwd.as_deref(),
        ),
        RouterRequestKind::AttachLookup { selector } => {
            attach_lookup(runtime_dir, registry, &selector)
        }
        RouterRequestKind::ListSessions => list_sessions(runtime_dir, registry),
        RouterRequestKind::KillSession { selector } => {
            kill_session(runtime_dir, registry, &selector)
        }
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
fn create_session(
    runtime_dir: &Path,
    registry: &mut Registry,
    events_tx: &Sender<RouterEvent>,
    profile: Option<&str>,
    cwd: Option<&Path>,
) -> RouterResult {
    let id = SessionId::new();
    let name = generate_name(NameKind::Session, |candidate| {
        name_is_taken(registry, candidate)
    });

    let mut child = match spawn_session_server(runtime_dir, id, &name, profile, cwd) {
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
    let report = match ready_rx.recv_timeout(READY_WAIT) {
        Ok(Some(report)) if report.protocol_version == ROUTER_PROTOCOL_VERSION => report,
        _ => {
            kill_child(&mut child);
            // A child that bound its socket before it was killed left an
            // endpoint file behind; this takes it back off the disk.
            unregister(runtime_dir, registry, id);
            return refused("the session did not report a bound socket".to_string());
        }
    };

    registry.insert(
        id,
        SessionEntry {
            name: name.clone(),
            socket: report.socket.clone(),
            pid,
            created_at: SystemTime::now(),
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
/// The address is probed before it is handed out: a probe that finds nothing
/// listening means the session server is gone, so the entry and the files it
/// left behind go with it.
fn attach_lookup(
    runtime_dir: &Path,
    registry: &mut Registry,
    selector: &SessionSelector,
) -> RouterResult {
    let Some(id) = resolve(registry, selector) else {
        return refused(no_such_session(selector));
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
            refused(no_such_session(selector))
        }
        Err(error) => refused(format!("the session could not be reached: {error}")),
    }
}

/// Describe every running session, in name then id order.
///
/// Each entry is asked to describe itself; one that does not answer is gone,
/// so it is removed and left out of the answer.
fn list_sessions(runtime_dir: &Path, registry: &mut Registry) -> RouterResult {
    let mut rows = Vec::new();
    let mut gone = Vec::new();
    for id in registry.keys().copied() {
        match ipc_client::fetch_overview(runtime_dir, id) {
            Ok(overview) => rows.push(overview.session),
            Err(_) => gone.push(id),
        }
    }
    for id in gone {
        unregister(runtime_dir, registry, id);
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    RouterResult::Sessions(rows)
}

/// End one session by forwarding the quit to its own server.
///
/// The entry stays: the child's exit, or the next probe that finds nothing
/// listening, is what removes it. A session that cannot be reached at all is
/// already gone, so it is removed here.
fn kill_session(
    runtime_dir: &Path,
    registry: &mut Registry,
    selector: &SessionSelector,
) -> RouterResult {
    let Some(id) = resolve(registry, selector) else {
        return refused(no_such_session(selector));
    };
    match ipc_client::submit_external_via_runtime_dir(runtime_dir, id, Command::Quit) {
        Ok(_) => RouterResult::Killed,
        Err(CliError::SessionNotFound { .. }) => {
            unregister(runtime_dir, registry, id);
            refused(no_such_session(selector))
        }
        Err(error) => refused(format!("the session could not be reached: {error}")),
    }
}

/// Rebuild the session list from what is already running.
///
/// Every advertised session is read for its address and process id and asked
/// to describe itself. One that answers both is registered — a session server
/// that outlived an earlier router is picked up here. One that fails either
/// is gone, so its endpoint file and socket are removed.
///
/// The walk is over endpoint files, which exist on every platform, so a
/// Windows pipe with no directory entry of its own is still found.
fn sweep(runtime_dir: &Path) -> Registry {
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
                        created_at: overview.session.created_at,
                    },
                );
            }
            _ => unregister(runtime_dir, &mut registry, id),
        }
    }
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

/// Drop one session from the list and remove what it advertised: its endpoint
/// file, and on Unix its socket file. Both are derived from the id, so this
/// works for an entry that was never in the list.
fn unregister(runtime_dir: &Path, registry: &mut Registry, id: SessionId) {
    registry.remove(&id);
    let _ = std::fs::remove_file(EndpointFile::path(runtime_dir, id));
    remove_socket_file(&socket_addr(runtime_dir, id));
}

/// Build the command that starts one session server: its identity on the
/// command line, its output piped back for the ready report, and the
/// directory its first shell opens in.
fn session_server_command(
    runtime_dir: &Path,
    id: SessionId,
    name: &str,
    profile: Option<&str>,
    cwd: Option<&Path>,
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
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    Ok(command)
}

/// Start one session server as a child of this router.
fn spawn_session_server(
    runtime_dir: &Path,
    id: SessionId,
    name: &str,
    profile: Option<&str>,
    cwd: Option<&Path>,
) -> std::io::Result<Child> {
    session_server_command(runtime_dir, id, name, profile, cwd)?.spawn()
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

/// A refusal to send back in place of a result.
// ponytail: the shared error-code list has no code for a request the router
// understood but cannot serve, so every refusal here carries the malformed
// code; give it its own code when one is added.
fn refused(message: String) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::MalformedRequest,
        message,
    })
}

/// The message for a selector naming a session the router does not have.
fn no_such_session(selector: &SessionSelector) -> String {
    match selector {
        SessionSelector::Id(id) => format!("no session {id} is running"),
        SessionSelector::Name(name) => format!("no session named `{name}` is running"),
    }
}
