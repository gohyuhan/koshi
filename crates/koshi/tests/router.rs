//! Cross-process tests for the router: a real `koshi` binary is started as the
//! router, it starts real session servers, and the tests speak the
//! control-plane protocol to it over its own socket.
//!
//! Every test serves its own temporary runtime directory, so the routers here
//! never meet the one a developer is running. The directory sits under a short
//! base because a Unix socket path has an operating-system length cap that a
//! deep temporary path would break.
//!
//! Every process a test starts is held in a guard that ends it when the test
//! drops it, so a failed assertion leaves nothing running.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use koshi_core::ids::SessionId;
use koshi_ipc::endpoint::{socket_addr, EndpointFile};
use koshi_ipc::protocol::{IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    router_endpoint_path, RouterRequest, RouterRequestKind, RouterResponse, RouterResult,
    SessionAddress, SessionSelector, MIN_ROUTER_PROTOCOL_VERSION, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;
use tempfile::TempDir;

/// How long a poll waits for something a started process has to do before the
/// test calls it a failure.
const WAIT: Duration = Duration::from_secs(20);

/// How long a poll waits for the router to end once no session is left. It has
/// to outlast the router's own idle window.
const EXIT_WAIT: Duration = Duration::from_secs(90);

/// How long a poll pauses between attempts.
const POLL: Duration = Duration::from_millis(100);

/// A fresh directory to serve, under a short base so the Unix socket path
/// stays inside the operating system's path-length cap. Removed when the test
/// drops it.
fn test_runtime_dir() -> TempDir {
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    TempDir::new_in(base).expect("a temporary runtime directory")
}

/// A router the test started. Dropping it ends that router.
struct RunningRouter(Child);

impl RunningRouter {
    /// True once the router process has ended.
    fn has_exited(&mut self) -> bool {
        self.0
            .try_wait()
            .expect("the router's state can be read")
            .is_some()
    }
}

impl Drop for RunningRouter {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The session servers a test made a router start. Dropping it ends them, so a
/// test that kills its router leaves no session server behind.
struct RunningSessions(Vec<u32>);

impl Drop for RunningSessions {
    fn drop(&mut self) {
        for pid in &self.0 {
            end_process(*pid);
        }
    }
}

/// Start the `koshi` binary as the router serving `runtime_dir`.
fn start_router(runtime_dir: &Path) -> RunningRouter {
    let child = Command::new(env!("CARGO_BIN_EXE_koshi"))
        .arg("serve-router")
        .arg("--runtime-dir")
        .arg(runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the koshi binary starts");
    RunningRouter(child)
}

/// End the process with id `pid`, whatever it is doing.
#[cfg(unix)]
fn end_process(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// End the process with id `pid`, whatever it is doing.
#[cfg(windows)]
fn end_process(pid: u32) {
    let _ = Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .arg("/F")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Open a connection to the router serving `runtime_dir`, with its handshake
/// already done, retrying until one answers.
fn router_connect(runtime_dir: &Path) -> Connection {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(connection) = try_router_connect(runtime_dir) {
            return connection;
        }
        assert!(
            Instant::now() < deadline,
            "no router answered in {}",
            runtime_dir.display()
        );
        std::thread::sleep(POLL);
    }
}

/// One attempt at opening a router connection: read the endpoint file,
/// connect, and send the Hello that opens the connection.
///
/// `None` means no router answered yet. A router that has just replaced
/// another writes its own endpoint file a moment after it binds, so a Hello
/// carrying the older file's token is refused; the next attempt reads the new
/// file.
fn try_router_connect(runtime_dir: &Path) -> Option<Connection> {
    let endpoint = EndpointFile::read(&router_endpoint_path(runtime_dir)).ok()?;
    let mut connection = Connection::connect(&endpoint.socket).ok()?;
    let hello = RouterRequest {
        request_id: 1,
        kind: RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            token: endpoint.token,
        },
    };
    connection.send(&hello).ok()?;
    let reply: RouterResponse = connection.recv().ok()?;
    match reply.result {
        RouterResult::Hello { .. } => Some(connection),
        RouterResult::Error(_) => None,
        other => panic!("the Hello was answered with {other:?}"),
    }
}

/// Ask the router `kind` on an open connection and hand back its answer.
fn request(connection: &mut Connection, kind: RouterRequestKind) -> RouterResult {
    let request = RouterRequest {
        request_id: 2,
        kind,
    };
    connection
        .send(&request)
        .expect("the router reads the request");
    let reply: RouterResponse = connection.recv().expect("the router answers the request");
    assert_eq!(reply.request_id, Some(2));
    reply.result
}

/// Ask the router for a new session and hand back where it listens.
fn create_session(connection: &mut Connection) -> SessionAddress {
    match request(
        connection,
        RouterRequestKind::CreateSession {
            profile: None,
            cwd: None,
        },
    ) {
        RouterResult::Created(address) => address,
        other => panic!("creating a session was answered with {other:?}"),
    }
}

/// Look one session up, and hand back the answer.
fn attach_lookup(connection: &mut Connection, selector: &SessionSelector) -> RouterResult {
    request(
        connection,
        RouterRequestKind::AttachLookup {
            selector: selector.clone(),
        },
    )
}

/// Look one session up until the router refuses it, and hand back that
/// refusal.
fn lookup_until_refused(connection: &mut Connection, selector: &SessionSelector) -> RouterResult {
    let deadline = Instant::now() + WAIT;
    loop {
        let result = attach_lookup(connection, selector);
        if matches!(result, RouterResult::Error(_)) {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "the session stayed in the router's list"
        );
        std::thread::sleep(POLL);
    }
}

/// The refusal the router answers a lookup with when it holds no such
/// session.
fn no_such_session(id: SessionId) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::MalformedRequest,
        message: format!("no session {id} is running"),
    })
}

#[test]
fn a_created_session_is_registered_and_found_by_its_id_and_by_its_name() {
    let dir = test_runtime_dir();
    let _router = start_router(dir.path());
    let mut connection = router_connect(dir.path());

    let created = create_session(&mut connection);
    let _sessions = RunningSessions(vec![created.pid]);

    // The router picks the name and the id, and the session server binds the
    // address those two derive.
    assert_eq!(created.name.split('-').next(), Some("S"));
    assert_eq!(created.socket, socket_addr(dir.path(), created.id));

    // The session server advertises the same address, under its own process
    // id — the one the router reported.
    let endpoint = EndpointFile::read(&EndpointFile::path(dir.path(), created.id))
        .expect("the session server advertises its socket");
    assert_eq!(endpoint.socket, created.socket);
    assert_eq!(endpoint.pid, created.pid);

    let by_id = attach_lookup(&mut connection, &SessionSelector::Id(created.id));
    assert_eq!(by_id, RouterResult::Found(created.clone()));

    let by_name = attach_lookup(
        &mut connection,
        &SessionSelector::Name(created.name.clone()),
    );
    assert_eq!(by_name, RouterResult::Found(created));
}

#[test]
fn a_session_server_that_is_killed_leaves_the_list_and_takes_its_files_with_it() {
    let dir = test_runtime_dir();
    let _router = start_router(dir.path());
    let mut connection = router_connect(dir.path());

    let created = create_session(&mut connection);
    let _sessions = RunningSessions(vec![created.pid]);

    end_process(created.pid);

    let refused = lookup_until_refused(&mut connection, &SessionSelector::Id(created.id));
    assert_eq!(refused, no_such_session(created.id));

    assert!(!EndpointFile::path(dir.path(), created.id).exists());
    #[cfg(unix)]
    assert!(!Path::new(&created.socket).exists());
}

#[test]
fn a_restarted_router_rediscovers_a_session_server_that_outlived_it() {
    let dir = test_runtime_dir();
    let first_router = start_router(dir.path());
    let mut connection = router_connect(dir.path());

    let created = create_session(&mut connection);
    let _sessions = RunningSessions(vec![created.pid]);

    // The router dies with no chance to tidy up; the session server it started
    // keeps serving its own socket.
    drop(connection);
    drop(first_router);

    let _second_router = start_router(dir.path());
    let mut connection = router_connect(dir.path());

    // The startup sweep read the session's name back from the session server
    // and its process id back from the endpoint file, so the answer is the one
    // the first router gave.
    let found = attach_lookup(&mut connection, &SessionSelector::Id(created.id));
    assert_eq!(found, RouterResult::Found(created.clone()));

    let overview = koshi::ipc_client::fetch_overview(dir.path(), created.id)
        .expect("the session server describes itself");
    assert_eq!(overview.session.id, created.id);
    assert_eq!(overview.session.name, created.name);
}

#[test]
fn two_routers_started_at_once_leave_exactly_one_running() {
    let dir = test_runtime_dir();
    let mut first = start_router(dir.path());
    let mut second = start_router(dir.path());

    // One of the two takes the lock and binds; the other finds the lock held
    // and exits without binding anything.
    let deadline = Instant::now() + WAIT;
    loop {
        let still_running = usize::from(!first.has_exited()) + usize::from(!second.has_exited());
        if still_running == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{still_running} of the two routers are running"
        );
        std::thread::sleep(POLL);
    }

    // The one left is the router, and it serves the socket both were started
    // to serve.
    let mut connection = router_connect(dir.path());
    assert_eq!(
        request(&mut connection, RouterRequestKind::ListSessions),
        RouterResult::Sessions(Vec::new())
    );
}

#[test]
fn an_adopted_session_server_that_dies_is_dropped_by_the_next_lookup() {
    let dir = test_runtime_dir();
    let first_router = start_router(dir.path());
    let mut connection = router_connect(dir.path());

    let created = create_session(&mut connection);
    let _sessions = RunningSessions(vec![created.pid]);

    drop(connection);
    drop(first_router);

    // The second router adopted this session through its startup sweep, so it
    // is not the session server's parent and no child exit reaches it.
    let _second_router = start_router(dir.path());
    let mut connection = router_connect(dir.path());
    assert_eq!(
        attach_lookup(&mut connection, &SessionSelector::Id(created.id)),
        RouterResult::Found(created.clone())
    );

    end_process(created.pid);

    // Nothing listens at the address any more, so the lookup that probes it
    // removes the session and the files it left behind.
    let refused = lookup_until_refused(&mut connection, &SessionSelector::Id(created.id));
    assert_eq!(refused, no_such_session(created.id));

    assert!(!EndpointFile::path(dir.path(), created.id).exists());
    #[cfg(unix)]
    assert!(!Path::new(&created.socket).exists());
}

#[test]
fn the_router_ends_itself_once_no_session_is_left() {
    let dir = test_runtime_dir();
    let mut router = start_router(dir.path());
    let mut connection = router_connect(dir.path());

    let created = create_session(&mut connection);
    let _sessions = RunningSessions(vec![created.pid]);

    // The router spawned this session server, so it is the parent and the
    // child's exit reaches it directly and empties the list.
    end_process(created.pid);
    drop(connection);

    // With the list empty the router waits one idle window for a request and
    // ends when none arrives.
    let deadline = Instant::now() + EXIT_WAIT;
    while !router.has_exited() {
        assert!(
            Instant::now() < deadline,
            "the router kept running with no session left"
        );
        std::thread::sleep(POLL);
    }

    assert!(!router_endpoint_path(dir.path()).exists());
}
