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
use koshi_ipc::endpoint::{compute_socket_address, EndpointFile};
use koshi_ipc::protocol::{IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    resolve_router_endpoint_path, RouterRequest, RouterRequestKind, RouterResponse, RouterResult,
    SessionAddress, SessionSelector, MIN_ROUTER_PROTOCOL_VERSION, ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::Connection;

mod common;

use common::{copy_koshi_binary, start_koshi_process, terminate_process};
use koshi_test_support::fixtures::build_test_runtime_directory;

/// How long a poll waits for something a started process has to do before the
/// test calls it a failure.
const WAIT_DURATION: Duration = Duration::from_secs(20);

/// How long a poll waits for the router to end once no session is left. It has
/// to outlast the router's own idle window.
const ROUTER_EXIT_WAIT_DURATION: Duration = Duration::from_secs(90);

/// How long a poll pauses between attempts.
const ROUTER_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(100);

/// A router the test started. Dropping it ends that router.
struct RunningRouter {
    child_process: Child,
}

impl RunningRouter {
    /// True once the router process has ended.
    fn has_router_exited(&mut self) -> bool {
        self.child_process
            .try_wait()
            .expect("the router's state can be read")
            .is_some()
    }
}

impl Drop for RunningRouter {
    fn drop(&mut self) {
        let _ = self.child_process.kill();
        let _ = self.child_process.wait();
    }
}

/// The session servers a test made a router start. Dropping it ends them, so a
/// test that kills its router leaves no session server behind.
struct RunningSessions {
    session_server_process_ids: Vec<u32>,
}

impl Drop for RunningSessions {
    fn drop(&mut self) {
        for process_id in &self.session_server_process_ids {
            terminate_process(*process_id);
        }
    }
}

/// A process the test did not start itself, held by its process id. Dropping
/// it ends that process.
#[cfg(windows)]
struct RunningProcess {
    process_id: u32,
}

#[cfg(windows)]
impl Drop for RunningProcess {
    fn drop(&mut self) {
        terminate_process(self.process_id);
    }
}

/// Start the `koshi` binary as the router serving `runtime_directory`.
fn start_router_process(runtime_directory: &Path) -> RunningRouter {
    start_router_from_binary(Path::new(env!("CARGO_BIN_EXE_koshi")), runtime_directory)
}

/// Start the binary at `binary_path` as the router serving `runtime_directory`.
fn start_router_from_binary(binary_path: &Path, runtime_directory: &Path) -> RunningRouter {
    let child_process = start_koshi_process(
        Command::new(binary_path)
            .arg("serve-router")
            .arg("--runtime-dir")
            .arg(runtime_directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    RunningRouter { child_process }
}

/// Open a connection to the router serving `runtime_directory`, with its handshake
/// already done, retrying until one answers.
fn connect_to_router(runtime_directory: &Path) -> Connection {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Some(connection) = try_connect_to_router(runtime_directory) {
            return connection;
        }
        assert!(
            Instant::now() < deadline,
            "no router answered in {}",
            runtime_directory.display()
        );
        std::thread::sleep(ROUTER_POLL_INTERVAL_DURATION);
    }
}

/// One attempt at opening a router connection: read the endpoint file,
/// connect, and send the Hello that opens the connection.
///
/// `None` means no router answered yet. A router that has just replaced
/// another writes its own endpoint file a moment after it binds, so a Hello
/// carrying the older file's token is refused; the next attempt reads the new
/// file.
fn try_connect_to_router(runtime_directory: &Path) -> Option<Connection> {
    let endpoint =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory)).ok()?;
    let mut connection = Connection::connect(&endpoint.socket_address).ok()?;
    let hello = RouterRequest {
        request_id: 1,
        request_kind: RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: endpoint.connection_token,
        },
    };
    connection.send(&hello).ok()?;
    let router_response: RouterResponse = connection.recv().ok()?;
    match router_response.answer_result {
        RouterResult::Hello { .. } => Some(connection),
        RouterResult::Error(_) => None,
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// The build version the running router reports in its Hello answer, or
/// `None` when no router answers.
fn get_router_hello_version(runtime_directory: &Path) -> Option<String> {
    let endpoint =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory)).ok()?;
    let mut connection = Connection::connect(&endpoint.socket_address).ok()?;
    let hello = RouterRequest {
        request_id: 1,
        request_kind: RouterRequestKind::Hello {
            min_protocol_version: MIN_ROUTER_PROTOCOL_VERSION,
            max_protocol_version: ROUTER_PROTOCOL_VERSION,
            connection_token: endpoint.connection_token,
        },
    };
    connection.send(&hello).ok()?;
    let router_response: RouterResponse = connection.recv().ok()?;
    match router_response.answer_result {
        RouterResult::Hello { build_version, .. } => Some(build_version),
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Ask the router for `request_kind` on an open connection and hand back its answer.
fn send_router_request(
    connection: &mut Connection,
    request_kind: RouterRequestKind,
) -> RouterResult {
    let router_request = RouterRequest {
        request_id: 2,
        request_kind,
    };
    connection
        .send(&router_request)
        .expect("the router reads the request");
    let router_response: RouterResponse =
        connection.recv().expect("the router answers the request");
    assert_eq!(router_response.request_id, Some(2));
    router_response.answer_result
}

/// Ask the router for a new session and hand back where it listens.
fn build_session(connection: &mut Connection) -> SessionAddress {
    match send_router_request(
        connection,
        RouterRequestKind::CreateSession {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
        },
    ) {
        RouterResult::Created(address) => address,
        unexpected_result => panic!("creating a session was answered with {unexpected_result:?}"),
    }
}

/// Look one session up, and hand back the answer.
fn lookup_session_for_attach(
    connection: &mut Connection,
    session_selector: &SessionSelector,
) -> RouterResult {
    send_router_request(
        connection,
        RouterRequestKind::AttachLookup {
            session_selector: session_selector.clone(),
        },
    )
}

/// Look one session up until the router refuses it, and hand back that
/// refusal.
fn wait_for_session_lookup_refusal(
    connection: &mut Connection,
    session_selector: &SessionSelector,
) -> RouterResult {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        let lookup_result = lookup_session_for_attach(connection, session_selector);
        if matches!(lookup_result, RouterResult::Error(_)) {
            return lookup_result;
        }
        assert!(
            Instant::now() < deadline,
            "the session stayed in the router's list"
        );
        std::thread::sleep(ROUTER_POLL_INTERVAL_DURATION);
    }
}

/// Assert the router lists exactly the session `created_session` names, and nothing
/// else.
fn assert_router_lists_only_session(connection: &mut Connection, created_session: &SessionAddress) {
    match send_router_request(connection, RouterRequestKind::ListSessions) {
        RouterResult::Sessions(sessions) => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, created_session.session_id);
            assert_eq!(sessions[0].session_name, created_session.session_name);
        }
        unexpected_result => panic!("listing the sessions was answered with {unexpected_result:?}"),
    }
}

/// The endpoint file the router serving `runtime_directory` writes after a restart,
/// waited for by the token it carries.
///
/// A router writes its endpoint file once its socket is bound, so an answer
/// here means the restarted router is ready for a connection. The token is
/// generated per router, so one other than `endpoint_before_restart`'s belongs to the
/// restarted one.
fn wait_for_restarted_router_endpoint(
    runtime_directory: &Path,
    endpoint_before_restart: &EndpointFile,
) -> EndpointFile {
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Ok(endpoint) =
            EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory))
        {
            if endpoint.connection_token.expose()
                != endpoint_before_restart.connection_token.expose()
            {
                return endpoint;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no router advertised itself after the restart"
        );
        std::thread::sleep(ROUTER_POLL_INTERVAL_DURATION);
    }
}

/// The refusal the router answers a lookup with when it holds no such
/// session.
fn build_no_such_session_result(session_id: SessionId) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::NotFound,
        message: format!("no session {session_id} is running"),
    })
}

#[test]
fn a_created_session_is_registered_and_found_by_its_id_and_by_its_name() {
    let runtime_directory = build_test_runtime_directory();
    let _router = start_router_process(runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = build_session(&mut connection);
    let _sessions = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    // The router picks the name and the id, and the session server binds the
    // address those two derive.
    assert_eq!(created_session.session_name.split('-').next(), Some("S"));
    assert_eq!(
        created_session.socket_address,
        compute_socket_address(runtime_directory.path(), created_session.session_id)
    );

    // The session server advertises the same address, under its own process
    // id — the one the router reported.
    let endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        created_session.session_id,
    ))
    .expect("the session server advertises its socket");
    assert_eq!(endpoint.socket_address, created_session.socket_address);
    assert_eq!(endpoint.process_id, created_session.process_id);

    let by_id = lookup_session_for_attach(
        &mut connection,
        &SessionSelector::SessionId(created_session.session_id),
    );
    assert_eq!(by_id, RouterResult::Found(created_session.clone()));

    let by_name = lookup_session_for_attach(
        &mut connection,
        &SessionSelector::SessionName(created_session.session_name.clone()),
    );
    assert_eq!(by_name, RouterResult::Found(created_session));
}

#[test]
fn a_session_server_that_is_killed_leaves_the_list_and_takes_its_files_with_it() {
    let runtime_directory = build_test_runtime_directory();
    let _router = start_router_process(runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = build_session(&mut connection);
    let _sessions = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    terminate_process(created_session.process_id);

    let refused = wait_for_session_lookup_refusal(
        &mut connection,
        &SessionSelector::SessionId(created_session.session_id),
    );
    assert_eq!(
        refused,
        build_no_such_session_result(created_session.session_id)
    );

    assert!(!EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        created_session.session_id
    )
    .exists());
    #[cfg(unix)]
    assert!(!Path::new(&created_session.socket_address).exists());
}

#[test]
fn a_restarted_router_rediscovers_a_session_server_that_outlived_it() {
    let runtime_directory = build_test_runtime_directory();
    let first_router = start_router_process(runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = build_session(&mut connection);
    let _sessions = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    // The router dies with no chance to tidy up; the session server it started
    // keeps serving its own socket.
    drop(connection);
    drop(first_router);

    let _second_router = start_router_process(runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    // The startup sweep read the session's name back from the session server
    // and its process id back from the endpoint file, so the answer is the one
    // the first router gave.
    let lookup_result = lookup_session_for_attach(
        &mut connection,
        &SessionSelector::SessionId(created_session.session_id),
    );
    assert_eq!(lookup_result, RouterResult::Found(created_session.clone()));

    let overview = koshi_link::ipc_client::fetch_session_overview(
        runtime_directory.path(),
        created_session.session_id,
    )
    .expect("the session server describes itself");
    assert_eq!(overview.session.session_id, created_session.session_id);
    assert_eq!(overview.session.session_name, created_session.session_name);
}

#[test]
fn two_routers_started_at_once_leave_exactly_one_running() {
    let runtime_directory = build_test_runtime_directory();
    let mut first_router_process = start_router_process(runtime_directory.path());
    let mut second_router_process = start_router_process(runtime_directory.path());

    // One of the two takes the lock and binds; the other finds the lock held
    // and exits without binding anything.
    let deadline = Instant::now() + WAIT_DURATION;
    loop {
        let running_router_count = usize::from(!first_router_process.has_router_exited())
            + usize::from(!second_router_process.has_router_exited());
        if running_router_count == 1 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{running_router_count} of the two routers are running"
        );
        std::thread::sleep(ROUTER_POLL_INTERVAL_DURATION);
    }

    // The one left is the router, and it serves the socket both were started
    // to serve.
    let mut connection = connect_to_router(runtime_directory.path());
    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::ListSessions),
        RouterResult::Sessions(Vec::new())
    );
}

#[test]
fn an_adopted_session_server_that_dies_is_dropped_by_the_next_lookup() {
    let runtime_directory = build_test_runtime_directory();
    let first_router = start_router_process(runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = build_session(&mut connection);
    let _sessions = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    drop(connection);
    drop(first_router);

    // The second router adopted this session through its startup sweep, so it
    // is not the session server's parent and no child exit reaches it.
    let _second_router = start_router_process(runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());
    assert_eq!(
        lookup_session_for_attach(
            &mut connection,
            &SessionSelector::SessionId(created_session.session_id)
        ),
        RouterResult::Found(created_session.clone())
    );

    terminate_process(created_session.process_id);

    // Nothing listens at the address any more, so the lookup that probes it
    // removes the session and the files it left behind.
    let refused = wait_for_session_lookup_refusal(
        &mut connection,
        &SessionSelector::SessionId(created_session.session_id),
    );
    assert_eq!(
        refused,
        build_no_such_session_result(created_session.session_id)
    );

    assert!(!EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        created_session.session_id
    )
    .exists());
    #[cfg(unix)]
    assert!(!Path::new(&created_session.socket_address).exists());
}

#[test]
fn the_router_ends_itself_once_no_session_is_left() {
    let runtime_directory = build_test_runtime_directory();
    let mut router = start_router_process(runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = build_session(&mut connection);
    let _sessions = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    // The router spawned this session server, so it is the parent and the
    // child's exit reaches it directly and empties the list.
    terminate_process(created_session.process_id);
    drop(connection);

    // With the list empty the router waits one idle window for a request and
    // ends when none arrives.
    let deadline = Instant::now() + ROUTER_EXIT_WAIT_DURATION;
    while !router.has_router_exited() {
        assert!(
            Instant::now() < deadline,
            "the router kept running with no session left"
        );
        std::thread::sleep(ROUTER_POLL_INTERVAL_DURATION);
    }

    assert!(!resolve_router_endpoint_path(runtime_directory.path()).exists());
}

#[test]
fn a_restart_keeps_the_sessions_and_the_router_serving() {
    let runtime_directory = build_test_runtime_directory();
    // The binary on disk never changes here, so the restart starts the same
    // program the router already runs.
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let mut router = start_router_from_binary(&binary_path, runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = build_session(&mut connection);
    let _sessions = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    let endpoint_before_restart =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the router advertises its socket");

    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let restarted_endpoint =
        wait_for_restarted_router_endpoint(runtime_directory.path(), &endpoint_before_restart);
    let mut connection = connect_to_router(runtime_directory.path());

    // The restarted router rebuilt its list from the endpoint files, so the
    // session started before the restart is still registered.
    assert_router_lists_only_session(&mut connection, &created_session);

    // The restarted router reports the build version of the binary it now
    // runs — the fact `koshi update` reads to confirm a restart.
    assert_eq!(
        get_router_hello_version(runtime_directory.path()),
        Some(env!("CARGO_PKG_VERSION").to_string())
    );

    #[cfg(unix)]
    {
        // The restart replaced this process's running image, so the router
        // still runs under the process id it started with.
        assert!(!router.has_router_exited());
        assert_eq!(
            restarted_endpoint.process_id,
            endpoint_before_restart.process_id
        );
    }
    #[cfg(windows)]
    let _restarted = {
        // The restart handed over to a new process, which took the lock the
        // old one released as it exited.
        let deadline = Instant::now() + WAIT_DURATION;
        while !router.has_router_exited() {
            assert!(
                Instant::now() < deadline,
                "the router that handed over kept running"
            );
            std::thread::sleep(ROUTER_POLL_INTERVAL_DURATION);
        }
        assert_ne!(
            restarted_endpoint.process_id,
            endpoint_before_restart.process_id
        );
        RunningProcess {
            process_id: restarted_endpoint.process_id,
        }
    };

    // The restarted router holds the lock, so a router started beside it
    // binds nothing and exits.
    let mut rival = start_router_from_binary(&binary_path, runtime_directory.path());
    let deadline = Instant::now() + WAIT_DURATION;
    while !rival.has_router_exited() {
        assert!(
            Instant::now() < deadline,
            "a second router kept running beside the restarted one"
        );
        std::thread::sleep(ROUTER_POLL_INTERVAL_DURATION);
    }
    assert_eq!(
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the restarted router still advertises its socket")
            .process_id,
        restarted_endpoint.process_id
    );
}

#[test]
fn a_restart_with_the_binary_gone_is_refused_and_the_old_router_keeps_serving() {
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let mut router = start_router_from_binary(&binary_path, runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = build_session(&mut connection);
    let _sessions = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    // A running program can be renamed on every supported platform, and that
    // is how an update moves the old binary aside.
    let moved_binary_path = binary_path.with_extension("moved");
    std::fs::rename(&binary_path, &moved_binary_path).expect("the binary is moved aside");
    let missing_binary_error =
        std::fs::metadata(&binary_path).expect_err("nothing is at that path");

    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!(
                "the binary at {} could not be read: {missing_binary_error}",
                binary_path.display()
            ),
        })
    );

    // Nothing was torn down for the refused restart: the connection that
    // asked for it still serves, and the router still runs.
    assert_router_lists_only_session(&mut connection, &created_session);
    assert!(!router.has_router_exited());

    std::fs::rename(&moved_binary_path, &binary_path).expect("the binary is put back");
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
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let mut router = start_router_from_binary(&binary_path, runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = build_session(&mut connection);
    let _sessions = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    std::fs::remove_file(&binary_path).expect("the binary is taken away");
    std::fs::create_dir(&binary_path).expect("a directory takes the binary's place");

    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    // A listing is answered by the dispatcher, and the dispatcher reads events
    // again only once the exec it ended for has returned. The answer is
    // therefore from the resumed router, the one the writes below reach.
    let mut connection = connect_to_router(runtime_directory.path());
    assert_router_lists_only_session(&mut connection, &created_session);

    // The exec failed, so the endpoint file is the one this router wrote when
    // it bound.
    let endpoint =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the router still advertises the socket it bound");
    for _ in 0..5 {
        let mut hangs_up =
            Connection::connect(&endpoint.socket_address).expect("the router accepts a connection");
        let hello = RouterRequest {
            request_id: 1,
            request_kind: RouterRequestKind::build_hello_request(endpoint.connection_token.clone()),
        };
        let listing = RouterRequest {
            request_id: 2,
            request_kind: RouterRequestKind::ListSessions,
        };
        hangs_up.send(&hello).expect("the router reads the Hello");
        hangs_up
            .send(&listing)
            .expect("the router reads the listing request");
        // Both answers are written into a socket whose peer has gone.
        drop(hangs_up);
        std::thread::sleep(ROUTER_POLL_INTERVAL_DURATION);
    }

    assert!(!router.has_router_exited());

    let mut connection = connect_to_router(runtime_directory.path());
    assert_router_lists_only_session(&mut connection, &created_session);
}

#[test]
fn a_restart_with_no_session_registered_comes_back_serving_an_empty_list() {
    // With no session running the dispatcher is inside its idle window, and a
    // delivered restart reply has to end that wait too.
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let _router = start_router_from_binary(&binary_path, runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let endpoint_before_restart =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the router advertises its socket");

    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let restarted_endpoint =
        wait_for_restarted_router_endpoint(runtime_directory.path(), &endpoint_before_restart);
    #[cfg(windows)]
    let _restarted = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };
    let mut connection = connect_to_router(runtime_directory.path());

    // The restarted router took back the address the old one served, so a
    // client finds it where it found the old one.
    assert_eq!(
        restarted_endpoint.socket_address,
        endpoint_before_restart.socket_address
    );
    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::ListSessions),
        RouterResult::Sessions(Vec::new())
    );
}

#[test]
fn a_router_that_restarted_restarts_again() {
    // The restarted router is a router in full: it holds the lock, serves the
    // socket, and answers a second restart the same way.
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let _router = start_router_from_binary(&binary_path, runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = build_session(&mut connection);
    let _sessions = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    let initial_endpoint =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the router advertises its socket");
    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let first_restart_endpoint =
        wait_for_restarted_router_endpoint(runtime_directory.path(), &initial_endpoint);
    #[cfg(windows)]
    let _restarted_once = RunningProcess {
        process_id: first_restart_endpoint.process_id,
    };
    let mut connection = connect_to_router(runtime_directory.path());

    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let second_restart_endpoint =
        wait_for_restarted_router_endpoint(runtime_directory.path(), &first_restart_endpoint);
    #[cfg(windows)]
    let _restarted_twice = RunningProcess {
        process_id: second_restart_endpoint.process_id,
    };
    #[cfg(unix)]
    // Both restarts replaced the running image, so the process id the router
    // started with is still the one serving.
    assert_eq!(
        second_restart_endpoint.process_id,
        initial_endpoint.process_id
    );
    let mut connection = connect_to_router(runtime_directory.path());

    // The session started before either restart is still registered.
    assert_router_lists_only_session(&mut connection, &created_session);
}
