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
//! leaves the list three ways: its reaper thread reports the child's exit, on
//! Unix a watcher thread reports the exit of a session the rebuild picked up,
//! or a lookup finds nothing listening at its address. All remove the entry
//! and, for a session this user started, the files it left behind.
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

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use fs4::{FileExt, TryLockError};
use koshi_core::ids::SessionId;
use koshi_core::naming::{generate_name, NameKind};
use koshi_ipc::endpoint::{remove_socket_file, socket_addr, EndpointFile};
use koshi_ipc::error::IpcError;
use koshi_ipc::protocol::{ConnectionToken, IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    router_endpoint_path, router_lock_path, router_socket_addr, IncomingRouterRequest,
    RouterHandshake, RouterRequestKind, RouterResponse, RouterResult, SessionAddress,
    SessionSelector, SessionServerReady, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::{Connection, Listener};
use koshi_ipc::validate::{reclaim_stale_socket, validate_socket_addr};
use koshi_ipc::wire::MaybeKnown;

use crate::ipc_client;
use crate::router_client::{ROUTER_SUBCOMMAND, RUNTIME_DIR_FLAG};

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

/// How long a replacement router waits for the previous router to release the
/// router lock. The operating system releases that lock if the previous router
/// dies.
const LOCK_HANDOVER_WAIT: Duration = Duration::from_secs(10);

/// How long the lock wait pauses between attempts on the router lock.
const LOCK_HANDOVER_POLL: Duration = Duration::from_millis(100);

/// How long shutdown pauses after withdrawing the socket, giving a serving
/// thread time to finish the reply it is writing.
const DRAIN_GRACE: Duration = Duration::from_millis(100);

/// The subcommand the router starts itself under to run one session server.
/// The arguments after it are the session id, the session name,
/// [`RUNTIME_DIR_FLAG`] with the directory this router serves,
/// [`PROFILE_FLAG`] when the create named a profile, and
/// [`ALLOW_OTHER_USERS_FLAG`] when the create asked for the other users of
/// this machine.
const SESSION_SERVER_SUBCOMMAND: &str = "serve-session";

/// The flag carrying a `--profile` name to the session server the router
/// starts, so the session opens that profile's tabs and panes.
const PROFILE_FLAG: &str = "--profile";

/// The flag telling the session server the router starts to let the other
/// users of this machine reach the session, whatever its `koshi.kdl` says.
const ALLOW_OTHER_USERS_FLAG: &str = "--allow-other-users";

/// The flag this router passes to the router it starts, telling that one to
/// wait for the router lock rather than yield to the router holding it.
#[cfg(windows)]
const WAIT_FOR_LOCK_FLAG: &str = "--wait-for-lock";

/// The Win32 `CREATE_NO_WINDOW` creation flag: the started process gets a
/// console with no window on screen.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The Win32 `DETACHED_PROCESS` creation flag: the started process gets no
/// console and does not inherit the caller's.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// The Win32 `CREATE_NEW_PROCESS_GROUP` creation flag: the started process
/// begins a process group of its own.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

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
    /// The `Restarting` reply has been written to its connection, so the
    /// router may now restart.
    RestartDelivered,
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
    let accept_thread = start_accept_thread(listener, token, events_tx.clone(), &shutting_down)?;

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
            &events_tx,
            &events_rx,
            ROUTER_IDLE_EXIT,
            &mut registry,
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

/// Block SIGPIPE on the calling thread's signal mask.
///
/// The blocked signal stays pending and is discarded when the thread ends; a
/// write to a hung-up peer returns an `EPIPE` error under every process-wide
/// disposition.
#[cfg(unix)]
fn block_sigpipe_on_this_thread() {
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// Replace this process's running image with the binary at `exe`, serving the
/// same runtime directory. The call returns only when the exec failed, and
/// hands back that error.
///
/// `exec` runs the command's setup steps and then, before calling `execvp`,
/// resets SIGPIPE to `SIG_DFL` in this process. It does that even with no
/// setup step configured on the command (the standard library's
/// `sys/process/unix/unix.rs`, in `do_exec`). A failed exec therefore puts
/// `SIG_IGN` back here before returning, so the resumed router keeps ignoring
/// the signal a write to a hung-up client raises.
///
/// The SIGPIPE reset is the only change this function undoes, so a setup step
/// added to the command must be undone here beside it.
///
/// A successful exec closes every descriptor the standard library opened
/// close-on-exec, the lock file among them, at the instant the old image ends.
/// The new image's [`run_router`] then takes the lock, reclaims the socket
/// path, binds, writes a fresh endpoint file, and rebuilds the session list —
/// under the same process id.
#[cfg(unix)]
fn restart_by_exec(exe: &Path, runtime_dir: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let error = std::process::Command::new(exe)
        .arg(ROUTER_SUBCOMMAND)
        .arg(RUNTIME_DIR_FLAG)
        .arg(runtime_dir)
        .exec();
    let _ = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    error
}

/// Start the binary at `exe` as a new router over the same runtime directory,
/// waiting for the lock this router still holds.
///
/// The new router is detached with a process group of its own and no console,
/// and its input and output go nowhere. An error means nothing was started.
#[cfg(windows)]
fn hand_over_to(exe: &Path, runtime_dir: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;

    std::process::Command::new(exe)
        .arg(ROUTER_SUBCOMMAND)
        .arg(RUNTIME_DIR_FLAG)
        .arg(runtime_dir)
        .arg(WAIT_FOR_LOCK_FLAG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
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
    block_sigpipe_on_this_thread();
    let mut gate = RouterHandshake::new(token);
    loop {
        let request: IncomingRouterRequest = match connection.recv() {
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
        // A kind this build does not have comes from a newer koshi. It is
        // refused by name and the connection keeps serving.
        let kind = match request.kind {
            MaybeKnown::Known(kind) => kind,
            MaybeKnown::Unknown { name } => {
                let refusal = RouterResponse {
                    request_id,
                    result: RouterResult::Error(gate.refuse_unknown(&name)),
                };
                if connection.send(&refusal).is_err() {
                    return;
                }
                continue;
            }
        };

        let response = match gate.check(&kind) {
            Err(refusal) => RouterResponse {
                request_id,
                result: RouterResult::Error(refusal),
            },
            Ok(()) => match kind {
                RouterRequestKind::Hello { .. } => RouterResponse {
                    request_id,
                    result: RouterResult::Hello {
                        protocol_version: gate
                            .agreed()
                            .expect("an accepted Hello settles the connection's version"),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                    },
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
/// thread so a child's exit reaches here.
fn dispatch(
    runtime_dir: &Path,
    exe: &Path,
    events_tx: &Sender<RouterEvent>,
    events_rx: &Receiver<RouterEvent>,
    idle_exit: Duration,
    registry: &mut Registry,
) -> RouterExit {
    loop {
        let received = if registry.is_empty() {
            events_rx.recv_timeout(idle_exit).map_err(|_| ())
        } else {
            events_rx.recv().map_err(|_| ())
        };
        let Ok(event) = received else {
            return RouterExit::Idle;
        };
        match event {
            RouterEvent::Request { kind, reply } => {
                let _ = reply.send(serve_request(runtime_dir, exe, registry, events_tx, kind));
            }
            RouterEvent::ChildExited(id) => unregister(runtime_dir, registry, id),
            RouterEvent::RestartDelivered => return RouterExit::Restart,
        }
    }
}

/// Answer one request against the session list.
fn serve_request(
    runtime_dir: &Path,
    exe: &Path,
    registry: &mut Registry,
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
    }
}

/// Answer a restart request by checking the binary at `exe`. A binary that
/// cannot be read is refused; on Unix, one with no execute permission is
/// refused too. Nothing is torn down either way.
fn restart_check(exe: &Path) -> RouterResult {
    match std::fs::metadata(exe) {
        Err(error) => refused(format!(
            "the binary at {} could not be read: {error}",
            exe.display()
        )),
        #[cfg(unix)]
        Ok(metadata) => {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o111 == 0 {
                refused(format!("the binary at {} is not executable", exe.display()))
            } else {
                RouterResult::Restarting
            }
        }
        #[cfg(not(unix))]
        Ok(_) => RouterResult::Restarting,
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

    let mut child =
        match spawn_session_server(runtime_dir, id, &name, profile, cwd, allow_other_users) {
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

/// Rebuild the session list from what is already running.
///
/// Every advertised session is read for its address and process id and asked
/// to describe itself. One that answers both is registered — a session server
/// that outlived an earlier router is picked up here. One that fails either
/// is gone, so its endpoint file and socket are removed.
///
/// The walk is over endpoint files, which exist on every platform, so a
/// Windows pipe with no directory entry of its own is still found.
///
/// `shared_base` is the machine-wide shared directory while
/// `allow-other-users` is on, and `None` while it is off. Each session it
/// advertises for another local user is asked over the address it names, and
/// registered when it answers. One that does not answer is left out and
/// nothing of it is removed: its files belong to that user.
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
                        created_at: overview.session.created_at,
                    },
                );
            }
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
                    created_at: overview.session.created_at,
                },
            );
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
/// works for an entry that was never in the list. A session another local user
/// started advertised neither of them here, so nothing of that user's is
/// removed.
fn unregister(runtime_dir: &Path, registry: &mut Registry, id: SessionId) {
    registry.remove(&id);
    let _ = std::fs::remove_file(EndpointFile::path(runtime_dir, id));
    remove_socket_file(&socket_addr(runtime_dir, id));
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

/// Start one session server as a child of this router.
fn spawn_session_server(
    runtime_dir: &Path,
    id: SessionId,
    name: &str,
    profile: Option<&str>,
    cwd: Option<&Path>,
    allow_other_users: Option<bool>,
) -> std::io::Result<Child> {
    session_server_command(runtime_dir, id, name, profile, cwd, allow_other_users)?.spawn()
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
