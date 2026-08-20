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

mod common;

use common::{copy_of_koshi, end_process, start_koshi};
use koshi_test_support::fixtures::test_runtime_dir;

/// How long a poll waits for something a started process has to do before the
/// test calls it a failure.
const WAIT: Duration = Duration::from_secs(20);

/// How long a poll waits for the router to end once no session is left. It has
/// to outlast the router's own idle window.
const EXIT_WAIT: Duration = Duration::from_secs(90);

/// How long a poll pauses between attempts.
const POLL: Duration = Duration::from_millis(100);

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

/// A process the test did not start itself, held by its process id. Dropping
/// it ends that process.
#[cfg(windows)]
struct RunningProcess(u32);

#[cfg(windows)]
impl Drop for RunningProcess {
    fn drop(&mut self) {
        end_process(self.0);
    }
}

/// Start the `koshi` binary as the router serving `runtime_dir`.
fn start_router(runtime_dir: &Path) -> RunningRouter {
    start_router_from(Path::new(env!("CARGO_BIN_EXE_koshi")), runtime_dir)
}

/// Start the binary at `exe` as the router serving `runtime_dir`.
fn start_router_from(exe: &Path, runtime_dir: &Path) -> RunningRouter {
    let child = start_koshi(
        Command::new(exe)
            .arg("serve-router")
            .arg("--runtime-dir")
            .arg(runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    RunningRouter(child)
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

/// The build version the running router reports in its Hello answer, or
/// `None` when no router answers.
fn hello_version(runtime_dir: &Path) -> Option<String> {
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
        RouterResult::Hello { version, .. } => Some(version),
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
            allow_other_users: None,
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

/// Assert the router lists exactly the session `created` names, and nothing
/// else.
fn assert_lists_only_the_session(connection: &mut Connection, created: &SessionAddress) {
    match request(connection, RouterRequestKind::ListSessions) {
        RouterResult::Sessions(sessions) => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].id, created.id);
            assert_eq!(sessions[0].name, created.name);
        }
        other => panic!("listing the sessions was answered with {other:?}"),
    }
}

/// The endpoint file the router serving `runtime_dir` writes after a restart,
/// waited for by the token it carries.
///
/// A router writes its endpoint file once its socket is bound, so an answer
/// here means the restarted router is ready for a connection. The token is
/// generated per router, so one other than `before`'s belongs to the
/// restarted one.
fn endpoint_after_restart(runtime_dir: &Path, before: &EndpointFile) -> EndpointFile {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(endpoint) = EndpointFile::read(&router_endpoint_path(runtime_dir)) {
            if endpoint.token.expose() != before.token.expose() {
                return endpoint;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no router advertised itself after the restart"
        );
        std::thread::sleep(POLL);
    }
}

/// The refusal the router answers a lookup with when it holds no such
/// session.
fn no_such_session(id: SessionId) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::NotFound,
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

    let overview = koshi_link::ipc_client::fetch_overview(dir.path(), created.id)
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

#[test]
fn a_restart_keeps_the_sessions_and_the_router_serving() {
    let dir = test_runtime_dir();
    // The binary on disk never changes here, so the restart starts the same
    // program the router already runs.
    let exe = copy_of_koshi(dir.path());
    let mut router = start_router_from(&exe, dir.path());
    let mut connection = router_connect(dir.path());

    let created = create_session(&mut connection);
    let _sessions = RunningSessions(vec![created.pid]);

    let before = EndpointFile::read(&router_endpoint_path(dir.path()))
        .expect("the router advertises its socket");

    assert_eq!(
        request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let after = endpoint_after_restart(dir.path(), &before);
    let mut connection = router_connect(dir.path());

    // The restarted router rebuilt its list from the endpoint files, so the
    // session started before the restart is still registered.
    assert_lists_only_the_session(&mut connection, &created);

    // The restarted router reports the build version of the binary it now
    // runs — the fact `koshi update` reads to confirm a restart.
    assert_eq!(
        hello_version(dir.path()),
        Some(env!("CARGO_PKG_VERSION").to_string())
    );

    #[cfg(unix)]
    {
        // The restart replaced this process's running image, so the router
        // still runs under the process id it started with.
        assert!(!router.has_exited());
        assert_eq!(after.pid, before.pid);
    }
    #[cfg(windows)]
    let _restarted = {
        // The restart handed over to a new process, which took the lock the
        // old one released as it exited.
        let deadline = Instant::now() + WAIT;
        while !router.has_exited() {
            assert!(
                Instant::now() < deadline,
                "the router that handed over kept running"
            );
            std::thread::sleep(POLL);
        }
        assert_ne!(after.pid, before.pid);
        RunningProcess(after.pid)
    };

    // The restarted router holds the lock, so a router started beside it
    // binds nothing and exits.
    let mut rival = start_router_from(&exe, dir.path());
    let deadline = Instant::now() + WAIT;
    while !rival.has_exited() {
        assert!(
            Instant::now() < deadline,
            "a second router kept running beside the restarted one"
        );
        std::thread::sleep(POLL);
    }
    assert_eq!(
        EndpointFile::read(&router_endpoint_path(dir.path()))
            .expect("the restarted router still advertises its socket")
            .pid,
        after.pid
    );
}

#[test]
fn a_restart_with_the_binary_gone_is_refused_and_the_old_router_keeps_serving() {
    let dir = test_runtime_dir();
    let exe = copy_of_koshi(dir.path());
    let mut router = start_router_from(&exe, dir.path());
    let mut connection = router_connect(dir.path());

    let created = create_session(&mut connection);
    let _sessions = RunningSessions(vec![created.pid]);

    // A running program can be renamed on every supported platform, and that
    // is how an update moves the old binary aside.
    let moved = exe.with_extension("moved");
    std::fs::rename(&exe, &moved).expect("the binary is moved aside");
    let missing = std::fs::metadata(&exe).expect_err("nothing is at that path");

    assert_eq!(
        request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!(
                "the binary at {} could not be read: {missing}",
                exe.display()
            ),
        })
    );

    // Nothing was torn down for the refused restart: the connection that
    // asked for it still serves, and the router still runs.
    assert_lists_only_the_session(&mut connection, &created);
    assert!(!router.has_exited());

    std::fs::rename(&moved, &exe).expect("the binary is put back");
}

/// A restart runs `exec`, which only Unix has. On Windows the restart starts a
/// new process instead, so no failed restart can leave this router's signal
/// handling changed.
///
/// The binary is replaced by a directory, which carries execute permission
/// and so passes the check before the exec; `execvp` of a directory then
/// fails with `EACCES`, because the process file is not an ordinary file.
#[cfg(unix)]
#[test]
fn a_failed_restart_leaves_the_router_serving_hung_up_clients() {
    let dir = test_runtime_dir();
    let exe = copy_of_koshi(dir.path());
    let mut router = start_router_from(&exe, dir.path());
    let mut connection = router_connect(dir.path());

    let created = create_session(&mut connection);
    let _sessions = RunningSessions(vec![created.pid]);

    std::fs::remove_file(&exe).expect("the binary is taken away");
    std::fs::create_dir(&exe).expect("a directory takes the binary's place");

    assert_eq!(
        request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    // A listing is answered by the dispatcher, and the dispatcher reads events
    // again only once the exec it ended for has returned. The answer is
    // therefore from the resumed router, the one the writes below reach.
    let mut connection = router_connect(dir.path());
    assert_lists_only_the_session(&mut connection, &created);

    // The exec failed, so the endpoint file is the one this router wrote when
    // it bound.
    let endpoint = EndpointFile::read(&router_endpoint_path(dir.path()))
        .expect("the router still advertises the socket it bound");
    for _ in 0..5 {
        let mut hangs_up =
            Connection::connect(&endpoint.socket).expect("the router accepts a connection");
        let hello = RouterRequest {
            request_id: 1,
            kind: RouterRequestKind::hello(endpoint.token.clone()),
        };
        let listing = RouterRequest {
            request_id: 2,
            kind: RouterRequestKind::ListSessions,
        };
        hangs_up.send(&hello).expect("the router reads the Hello");
        hangs_up
            .send(&listing)
            .expect("the router reads the listing request");
        // Both answers are written into a socket whose peer has gone.
        drop(hangs_up);
        std::thread::sleep(POLL);
    }

    assert!(!router.has_exited());

    let mut connection = router_connect(dir.path());
    assert_lists_only_the_session(&mut connection, &created);
}

#[test]
fn a_restart_with_no_session_registered_comes_back_serving_an_empty_list() {
    // With no session running the dispatcher is inside its idle window, and a
    // delivered restart reply has to end that wait too.
    let dir = test_runtime_dir();
    let exe = copy_of_koshi(dir.path());
    let _router = start_router_from(&exe, dir.path());
    let mut connection = router_connect(dir.path());

    let before = EndpointFile::read(&router_endpoint_path(dir.path()))
        .expect("the router advertises its socket");

    assert_eq!(
        request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let after = endpoint_after_restart(dir.path(), &before);
    #[cfg(windows)]
    let _restarted = RunningProcess(after.pid);
    let mut connection = router_connect(dir.path());

    // The restarted router took back the address the old one served, so a
    // client finds it where it found the old one.
    assert_eq!(after.socket, before.socket);
    assert_eq!(
        request(&mut connection, RouterRequestKind::ListSessions),
        RouterResult::Sessions(Vec::new())
    );
}

#[test]
fn a_router_that_restarted_restarts_again() {
    // The restarted router is a router in full: it holds the lock, serves the
    // socket, and answers a second restart the same way.
    let dir = test_runtime_dir();
    let exe = copy_of_koshi(dir.path());
    let _router = start_router_from(&exe, dir.path());
    let mut connection = router_connect(dir.path());

    let created = create_session(&mut connection);
    let _sessions = RunningSessions(vec![created.pid]);

    let first = EndpointFile::read(&router_endpoint_path(dir.path()))
        .expect("the router advertises its socket");
    assert_eq!(
        request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let second = endpoint_after_restart(dir.path(), &first);
    #[cfg(windows)]
    let _restarted_once = RunningProcess(second.pid);
    let mut connection = router_connect(dir.path());

    assert_eq!(
        request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let third = endpoint_after_restart(dir.path(), &second);
    #[cfg(windows)]
    let _restarted_twice = RunningProcess(third.pid);
    #[cfg(unix)]
    // Both restarts replaced the running image, so the process id the router
    // started with is still the one serving.
    assert_eq!(third.pid, first.pid);
    let mut connection = router_connect(dir.path());

    // The session started before either restart is still registered.
    assert_lists_only_the_session(&mut connection, &created);
}
