//! Cross-process tests for the router: a real `koshi` binary is started as the
//! router, it starts real session servers, and the tests speak the
//! control-plane protocol to it over its own socket.
//!
//! Every test serves its own temporary runtime directory under a short base.
//! Every router runs under its own temporary home, and so does every session
//! server it starts. Every process a test starts is held in a guard that ends
//! it when the test drops it.

use std::path::Path;
use std::time::{Duration, Instant};

use koshi_core::ids::SessionId;
use koshi_ipc::endpoint::{compute_socket_address, EndpointFile, ServerProgramFile};
use koshi_ipc::protocol::{IpcErrorCode, IpcErrorPayload};
use koshi_ipc::router::{
    resolve_router_endpoint_path, resolve_router_program_file_path, RouterRequest,
    RouterRequestKind, RouterResponse, RouterResult, SessionAddress, SessionSelector,
};
use koshi_ipc::transport::Connection;

mod common;

#[cfg(windows)]
use common::RunningProcess;
use common::{
    build_no_such_session_result, build_short_test_directory, connect_to_router, copy_koshi_binary,
    create_session, send_attach_lookup, send_router_request, start_router_from_binary,
    start_router_process, terminate_process, wait_for_session_lookup_refusal, RunningSessions,
    POLL_INTERVAL_DURATION, WAIT_DURATION,
};
use koshi_test_support::fixtures::build_test_runtime_directory;

/// How long a poll waits for the router to end once no session is left: 90
/// seconds, longer than the router's own idle window.
const ROUTER_EXIT_WAIT_DURATION: Duration = Duration::from_secs(90);

/// The build version the running router reports in its Hello answer, or
/// `None` when no router answers.
fn find_router_hello_version(runtime_directory: &Path) -> Option<String> {
    let router_endpoint =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory)).ok()?;
    let mut connection = Connection::connect(&router_endpoint.socket_address).ok()?;
    let hello_request = RouterRequest {
        request_id: 1,
        request_kind: RouterRequestKind::build_hello_request(router_endpoint.connection_token),
    };
    connection.send(&hello_request).ok()?;
    let router_response: RouterResponse = connection.recv().ok()?;
    match router_response.answer_result {
        RouterResult::Hello { build_version, .. } => Some(build_version),
        unexpected_result => panic!("the Hello was answered with {unexpected_result:?}"),
    }
}

/// Assert a lookup of `created_session`'s id is answered with `created_session`
/// whole: its id, name, address and process id.
fn assert_router_finds_session(connection: &mut Connection, created_session: &SessionAddress) {
    assert_eq!(
        send_attach_lookup(
            connection,
            &SessionSelector::SessionId(created_session.session_id)
        ),
        RouterResult::Found(created_session.clone())
    );
}

/// Read the router endpoint file in `runtime_directory` every
/// [`POLL_INTERVAL_DURATION`], and hand back the first one whose token differs
/// from `endpoint_before_restart`'s. A router writes its endpoint file once its
/// socket is bound, under a token of its own.
///
/// # Panics
/// When no such file reads within [`WAIT_DURATION`].
fn wait_for_restarted_router_endpoint(
    runtime_directory: &Path,
    endpoint_before_restart: &EndpointFile,
) -> EndpointFile {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        if let Ok(advertised_endpoint) =
            EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory))
        {
            if advertised_endpoint.connection_token.expose_secret()
                != endpoint_before_restart.connection_token.expose_secret()
            {
                return advertised_endpoint;
            }
        }
        assert!(
            Instant::now() < wait_deadline,
            "no router advertised itself after the restart"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
}

#[test]
fn a_created_session_is_registered_and_found_by_its_id_and_by_its_name() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let _router_process =
        start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    // The router picks the name and the id, and the session server binds the
    // address those two derive.
    let session_name_parts: Vec<&str> = created_session.session_name.split('-').collect();
    assert_eq!(
        session_name_parts.len(),
        3,
        "{}",
        created_session.session_name
    );
    assert_eq!(session_name_parts[0], "S");
    assert_eq!(
        created_session.socket_address,
        compute_socket_address(runtime_directory.path(), created_session.session_id)
    );

    // The session server advertises the same address, under its own process
    // id — the one the router reported.
    let session_endpoint = EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        created_session.session_id,
    ))
    .expect("the session server advertises its socket");
    assert_eq!(
        session_endpoint.socket_address,
        created_session.socket_address
    );
    assert_eq!(session_endpoint.process_id, created_session.process_id);

    let lookup_by_id_result = send_attach_lookup(
        &mut connection,
        &SessionSelector::SessionId(created_session.session_id),
    );
    assert_eq!(
        lookup_by_id_result,
        RouterResult::Found(created_session.clone())
    );

    let lookup_by_name_result = send_attach_lookup(
        &mut connection,
        &SessionSelector::SessionName(created_session.session_name.clone()),
    );
    assert_eq!(lookup_by_name_result, RouterResult::Found(created_session));
}

#[test]
fn a_session_server_that_is_killed_leaves_the_list_and_takes_its_files_with_it() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let _router_process =
        start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    terminate_process(created_session.process_id);

    let lookup_refusal = wait_for_session_lookup_refusal(
        &mut connection,
        &SessionSelector::SessionId(created_session.session_id),
    );
    assert_eq!(
        lookup_refusal,
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
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let first_router = start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    // The router dies with no chance to tidy up; the session server it started
    // keeps serving its own socket.
    drop(connection);
    drop(first_router);

    let _second_router = start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    // The startup sweep read the session's name from the session server and
    // its process id from the endpoint file: the answer equals the one the
    // first router gave.
    let lookup_result = send_attach_lookup(
        &mut connection,
        &SessionSelector::SessionId(created_session.session_id),
    );
    assert_eq!(lookup_result, RouterResult::Found(created_session.clone()));

    let session_overview = koshi_link::discovery::fetch_session_overview(
        runtime_directory.path(),
        None,
        created_session.session_id,
        None,
    )
    .expect("the session server describes itself");
    assert_eq!(
        session_overview.session.session_id,
        created_session.session_id
    );
    assert_eq!(
        session_overview.session.session_name,
        created_session.session_name
    );
}

#[test]
fn two_routers_started_at_once_leave_exactly_one_running() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let mut first_router_process =
        start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut second_router_process =
        start_router_process(test_home_directory.path(), runtime_directory.path());

    // One of the two takes the lock and binds; the other finds the lock held
    // and exits without binding anything.
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        let running_router_count = usize::from(!first_router_process.has_router_exited())
            + usize::from(!second_router_process.has_router_exited());
        if running_router_count == 1 {
            break;
        }
        assert!(
            Instant::now() < wait_deadline,
            "{running_router_count} of the two routers are running"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }

    // The one left is the router, and it serves the socket both were started
    // to serve.
    let mut connection = connect_to_router(runtime_directory.path());
    let missing_session_id = SessionId::new();
    assert_eq!(
        send_attach_lookup(
            &mut connection,
            &SessionSelector::SessionId(missing_session_id)
        ),
        build_no_such_session_result(missing_session_id)
    );
}

#[test]
fn a_list_sessions_request_is_refused_by_name_and_the_connection_keeps_serving() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let _router_process =
        start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    connection
        .send(&serde_json::json!({ "request_id": 2, "request_kind": "ListSessions" }))
        .expect("the router reads the request");
    let router_response: RouterResponse =
        connection.recv().expect("the router answers the request");
    assert_eq!(
        router_response,
        RouterResponse {
            request_id: Some(2),
            answer_result: RouterResult::Error(IpcErrorPayload {
                code: IpcErrorCode::UnsupportedKind,
                message: "this router has no request kind named ListSessions".to_string(),
            }),
        }
    );

    let missing_session_id = SessionId::new();
    assert_eq!(
        send_attach_lookup(
            &mut connection,
            &SessionSelector::SessionId(missing_session_id)
        ),
        build_no_such_session_result(missing_session_id)
    );
}

#[test]
fn an_adopted_session_server_that_dies_is_dropped_by_the_next_lookup() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let first_router = start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    drop(connection);
    drop(first_router);

    // The second router adopted this session through its startup sweep: it is
    // not the session server's parent, and no child exit reaches it.
    let _second_router = start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());
    assert_eq!(
        send_attach_lookup(
            &mut connection,
            &SessionSelector::SessionId(created_session.session_id)
        ),
        RouterResult::Found(created_session.clone())
    );

    terminate_process(created_session.process_id);

    // Nothing listens at the address any more: the lookup that probes it
    // removes the session and the files it left behind.
    let lookup_refusal = wait_for_session_lookup_refusal(
        &mut connection,
        &SessionSelector::SessionId(created_session.session_id),
    );
    assert_eq!(
        lookup_refusal,
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
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let mut router_process =
        start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    // The router spawned this session server: the child's exit reaches the
    // router directly and empties the list.
    terminate_process(created_session.process_id);
    drop(connection);

    // With the list empty the router waits one idle window for a request and
    // ends when none arrives.
    let exit_deadline = Instant::now() + ROUTER_EXIT_WAIT_DURATION;
    while !router_process.has_router_exited() {
        assert!(
            Instant::now() < exit_deadline,
            "the router kept running with no session left"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }

    assert!(!resolve_router_endpoint_path(runtime_directory.path()).exists());
    assert!(!resolve_router_program_file_path(runtime_directory.path()).exists());
}

#[test]
fn a_restart_keeps_the_sessions_and_the_router_serving() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    // The copied program file never changes here: the restart starts the same
    // program the router already runs.
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let mut router_process = start_router_from_binary(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
    );
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
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

    // The restarted router rebuilt its list from the endpoint files: the
    // session started before the restart is still registered.
    assert_router_finds_session(&mut connection, &created_session);

    // The restarted router reports the build version of the binary it now
    // runs — the fact `koshi update` reads to confirm a restart.
    assert_eq!(
        find_router_hello_version(runtime_directory.path()),
        Some(env!("CARGO_PKG_VERSION").to_string())
    );

    // The restarted router's program file names its process, its version,
    // and the binary it restarts into.
    let restarted_program_file = ServerProgramFile::load_from_path(
        &resolve_router_program_file_path(runtime_directory.path()),
    )
    .expect("the program file reads")
    .expect("the restarted router writes its program file");
    assert_eq!(
        (
            restarted_program_file.process_id,
            restarted_program_file.build_version.as_str(),
        ),
        (restarted_endpoint.process_id, env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        std::fs::canonicalize(&restarted_program_file.program_path)
            .expect("the program path names a file"),
        std::fs::canonicalize(&binary_path).expect("the copied binary resolves")
    );

    #[cfg(unix)]
    {
        // The restart replaced this process's running image: the router still
        // runs under the process id it started with.
        assert!(!router_process.has_router_exited());
        assert_eq!(
            restarted_endpoint.process_id,
            endpoint_before_restart.process_id
        );
    }
    #[cfg(windows)]
    let _restarted_router_process = {
        // The restart handed over to a new process, which took the lock the
        // old one released as it exited.
        let handoff_deadline = Instant::now() + WAIT_DURATION;
        while !router_process.has_router_exited() {
            assert!(
                Instant::now() < handoff_deadline,
                "the router that handed over kept running"
            );
            std::thread::sleep(POLL_INTERVAL_DURATION);
        }
        assert_ne!(
            restarted_endpoint.process_id,
            endpoint_before_restart.process_id
        );
        RunningProcess {
            process_id: restarted_endpoint.process_id,
        }
    };

    // The restarted router holds the lock: a router started beside it binds
    // nothing and exits.
    let mut rival_router_process = start_router_from_binary(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
    );
    let rival_exit_deadline = Instant::now() + WAIT_DURATION;
    while !rival_router_process.has_router_exited() {
        assert!(
            Instant::now() < rival_exit_deadline,
            "a second router kept running beside the restarted one"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
    assert_eq!(
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the restarted router still advertises its socket")
            .process_id,
        restarted_endpoint.process_id
    );
}

/// A session server whose endpoint file is gone when the router restarts, as
/// a session server that is ending removes it before it exits, is not listed
/// by the restarted router. The restarted router still reaps it once it
/// exits: no zombie stays behind. The router still runs after the reap, so no
/// other process reaped the session server.
#[cfg(unix)]
#[test]
fn a_session_server_the_restarted_router_does_not_list_is_reaped_once_it_exits() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let mut router_process = start_router_from_binary(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
    );
    let mut connection = connect_to_router(runtime_directory.path());
    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };
    std::fs::remove_file(EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        created_session.session_id,
    ))
    .expect("the session's endpoint file is removed");
    let endpoint_before_restart =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the router advertises its socket");

    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);
    wait_for_restarted_router_endpoint(runtime_directory.path(), &endpoint_before_restart);
    let session_unix_process_id =
        libc::pid_t::try_from(created_session.process_id).expect("a process id fits a pid");
    assert_eq!(
        unsafe { libc::kill(session_unix_process_id, libc::SIGKILL) },
        0,
        "the session server is ended"
    );

    // `kill` with signal `0` answers `0` for a running process and for a
    // zombie, and `ESRCH` once the process is reaped.
    let reap_deadline = Instant::now() + WAIT_DURATION;
    while unsafe { libc::kill(session_unix_process_id, 0) } == 0 {
        assert!(
            Instant::now() < reap_deadline,
            "the session server stayed a zombie of the restarted router"
        );
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH),
        "the session server is gone, not merely unreachable"
    );
    assert!(
        !router_process.has_router_exited(),
        "the restarted router still runs, so it reaped the session server"
    );
}

#[test]
fn a_restart_with_the_binary_gone_is_refused_and_the_old_router_keeps_serving() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let mut router_process = start_router_from_binary(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
    );
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
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
            code: IpcErrorCode::RequestFailed,
            message: format!(
                "the binary at {} could not be read: {missing_binary_error}",
                binary_path.display()
            ),
        })
    );

    // Nothing was torn down for the refused restart: the connection that
    // asked for it still serves, and the router still runs.
    assert_router_finds_session(&mut connection, &created_session);
    assert!(!router_process.has_router_exited());

    std::fs::rename(&moved_binary_path, &binary_path).expect("the binary is put back");
}

/// Unix only: the restart runs `execvp`. The program file is replaced by a
/// directory, which carries execute permission and passes the check before
/// the exec; `execvp` of a directory fails with `EACCES`. The router serves
/// on, and five clients that hang up before their answers are written leave
/// it serving.
#[cfg(unix)]
#[test]
fn a_failed_restart_leaves_the_router_serving_hung_up_clients() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let mut router_process = start_router_from_binary(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
    );
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
        session_server_process_ids: vec![created_session.process_id],
    };

    std::fs::remove_file(&binary_path).expect("the binary is taken away");
    std::fs::create_dir(&binary_path).expect("a directory takes the binary's place");

    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    // A lookup is answered by the dispatcher, and the dispatcher reads events
    // again only once the exec it ended for has returned. The answer is
    // therefore from the resumed router, the one the writes below reach.
    let mut connection = connect_to_router(runtime_directory.path());
    assert_router_finds_session(&mut connection, &created_session);

    // The exec failed: the endpoint file is the one this router wrote when it
    // bound.
    let router_endpoint_file =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the router still advertises the socket it bound");
    for _ in 0..5 {
        let mut hanging_up_connection = Connection::connect(&router_endpoint_file.socket_address)
            .expect("the router accepts a connection");
        let hello_request = RouterRequest {
            request_id: 1,
            request_kind: RouterRequestKind::build_hello_request(
                router_endpoint_file.connection_token.clone(),
            ),
        };
        let lookup_request = RouterRequest {
            request_id: 2,
            request_kind: RouterRequestKind::AttachLookup {
                session_selector: SessionSelector::SessionId(created_session.session_id),
            },
        };
        hanging_up_connection
            .send(&hello_request)
            .expect("the router reads the Hello");
        hanging_up_connection
            .send(&lookup_request)
            .expect("the router reads the lookup request");
        // Both answers are written into a socket whose peer has gone.
        drop(hanging_up_connection);
        std::thread::sleep(POLL_INTERVAL_DURATION);
    }

    assert!(!router_process.has_router_exited());

    let mut connection = connect_to_router(runtime_directory.path());
    assert_router_finds_session(&mut connection, &created_session);
}

#[test]
fn a_restart_with_no_session_registered_comes_back_answering_lookups() {
    // With no session running the dispatcher is inside its idle window, and a
    // delivered restart reply has to end that wait too.
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let _router_process = start_router_from_binary(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
    );
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
    let _restarted_router_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };
    let mut connection = connect_to_router(runtime_directory.path());

    // The restarted router took back the address the old one served: a client
    // finds it where it found the old one.
    assert_eq!(
        restarted_endpoint.socket_address,
        endpoint_before_restart.socket_address
    );
    let missing_session_id = SessionId::new();
    assert_eq!(
        send_attach_lookup(
            &mut connection,
            &SessionSelector::SessionId(missing_session_id)
        ),
        build_no_such_session_result(missing_session_id)
    );
}

#[test]
fn a_router_that_restarted_restarts_again() {
    // The restarted router is a router in full: it holds the lock, serves the
    // socket, and answers a second restart the same way.
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let _router_process = start_router_from_binary(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
    );
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
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
    // Both restarts replaced the running image: the process id the router
    // started with is still the one serving.
    assert_eq!(
        second_restart_endpoint.process_id,
        initial_endpoint.process_id
    );
    let mut connection = connect_to_router(runtime_directory.path());

    // The session started before either restart is still registered.
    assert_router_finds_session(&mut connection, &created_session);
}
