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
    resolve_router_endpoint_path, resolve_router_lock_path, resolve_router_program_file_path,
    RouterRequest, RouterRequestKind, RouterResponse, RouterResult, SessionAddress,
    SessionSelector,
};
use koshi_ipc::transport::Connection;

mod common;

#[cfg(windows)]
use common::RunningProcess;
use common::{
    build_koshi_command_under_home, build_no_such_session_result, build_short_test_directory,
    connect_to_router, copy_koshi_binary, create_session, resolve_log_directory_under_home,
    resolve_runtime_directory_under_home, send_attach_lookup, send_router_request,
    start_router_from_binary, start_router_process, terminate_process,
    wait_for_session_lookup_refusal, write_test_config, RunningRouter, RunningSessions,
    POLL_INTERVAL_DURATION, WAIT_DURATION,
};
use koshi_test_support::fixtures::build_test_runtime_directory;

/// How long a poll waits for the router to end once no session is left: 90
/// seconds, longer than the router's own idle window.
const ROUTER_EXIT_WAIT_DURATION: Duration = Duration::from_secs(90);

/// How long the lock test holds the router lock before it lets go: 1 second.
const HELD_ROUTER_LOCK_DURATION: Duration = Duration::from_secs(1);

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
/// `router_process` is the router the test started. On Unix the restart keeps
/// its process. On Windows the restart hands over to a new process, and
/// `router_process` ends as part of it.
///
/// # Panics
/// - On Unix, once `router_process` has ended and the endpoint file read after
///   that still names no other token. The message names the exit status and
///   the last lines of the router's error stream.
/// - When no such file reads within [`WAIT_DURATION`]. The message names what
///   the last read of the endpoint file found and the last lines of the
///   router's error stream.
fn wait_for_restarted_router_endpoint(
    runtime_directory: &Path,
    endpoint_before_restart: &EndpointFile,
    router_process: &mut RunningRouter,
) -> EndpointFile {
    let wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        #[cfg(unix)]
        let router_exit_status = router_process.find_router_exit_status();
        let last_endpoint_read =
            match EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory)) {
                Ok(advertised_endpoint)
                    if advertised_endpoint.connection_token.expose_secret()
                        != endpoint_before_restart.connection_token.expose_secret() =>
                {
                    return advertised_endpoint;
                }
                Ok(advertised_endpoint) => format!(
                    "the endpoint file still holds the token from before the restart, for \
                     process {}",
                    advertised_endpoint.process_id
                ),
                Err(endpoint_read_error) => {
                    format!("the endpoint file could not be read: {endpoint_read_error}")
                }
            };
        #[cfg(unix)]
        if let Some(router_exit_status) = router_exit_status {
            router_process.wait_for_error_stream_end(wait_deadline);
            panic!(
                "the router exited with {router_exit_status} before it advertised itself after the \
                 restart; its error stream ended with:\n{}",
                router_process.format_error_tail()
            );
        }
        assert!(
            Instant::now() < wait_deadline,
            "no router advertised itself after the restart within {WAIT_DURATION:?}: \
             {last_endpoint_read}; the router's error stream ended with:\n{}",
            router_process.format_error_tail()
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
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
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
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
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
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
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
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
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
fn a_command_run_while_an_ending_router_holds_the_lock_reaches_the_router_started_after_it() {
    // The test holds the router lock for 1 second with no endpoint file, as a
    // router that is ending does. The first router the command starts finds
    // the lock held and ends. The second one waits for the lock, takes it
    // once the test lets go, and answers the command.
    let test_home_directory = build_short_test_directory();
    let runtime_directory = resolve_runtime_directory_under_home(test_home_directory.path());
    std::fs::create_dir_all(&runtime_directory).expect("create the runtime folder");
    let router_lock_file = std::fs::File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(resolve_router_lock_path(&runtime_directory))
        .expect("open the router lock file");
    router_lock_file.try_lock().expect("take the router lock");
    let lock_holder_thread = std::thread::spawn(move || {
        std::thread::sleep(HELD_ROUTER_LOCK_DURATION);
        drop(router_lock_file);
    });

    let process_output = build_koshi_command_under_home(test_home_directory.path())
        .args(["share", "list", "--format", "json"])
        .output()
        .expect("the koshi binary runs");
    lock_holder_thread.join().expect("the lock holder ends");
    let router_endpoint =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(&runtime_directory))
            .expect("a router advertises itself");
    terminate_process(router_endpoint.process_id);

    assert_eq!(String::from_utf8_lossy(&process_output.stderr), "");
    assert_eq!(String::from_utf8_lossy(&process_output.stdout), "[]\n");
    assert_eq!(process_output.status.code(), Some(0));
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
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
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
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
    };

    let endpoint_before_restart =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the router advertises its socket");

    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let restarted_endpoint = wait_for_restarted_router_endpoint(
        runtime_directory.path(),
        &endpoint_before_restart,
        &mut router_process,
    );
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
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
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
    wait_for_restarted_router_endpoint(
        runtime_directory.path(),
        &endpoint_before_restart,
        &mut router_process,
    );
    let session_unix_process_id =
        libc::pid_t::try_from(created_session.process_id).expect("a process id fits a pid");
    assert_eq!(
        // SAFETY: `kill` takes the process id and the signal by value and
        // reads no memory.
        unsafe { libc::kill(session_unix_process_id, libc::SIGKILL) },
        0,
        "the session server is ended"
    );

    // `kill` with signal `0` answers `0` for a running process and for a
    // zombie, and `ESRCH` once the process is reaped.
    let reap_deadline = Instant::now() + WAIT_DURATION;
    // SAFETY: signal `0` sends nothing and reads no memory.
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
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
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
    use koshi_ipc::router::ROUTER_RESTARTING_MESSAGE;

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
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
    };

    std::fs::remove_file(&binary_path).expect("the binary is taken away");
    std::fs::create_dir(&binary_path).expect("a directory takes the binary's place");

    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    // Until the failed exec reopens the router's intake, a lookup is refused
    // with the restarting sentence. Each attempt uses a new connection, and
    // the first other answer comes from the resumed router.
    let resume_wait_deadline = Instant::now() + WAIT_DURATION;
    loop {
        let mut connection = connect_to_router(runtime_directory.path());
        match send_attach_lookup(
            &mut connection,
            &SessionSelector::SessionId(created_session.session_id),
        ) {
            RouterResult::Error(router_refusal)
                if router_refusal.message == ROUTER_RESTARTING_MESSAGE =>
            {
                assert!(
                    Instant::now() < resume_wait_deadline,
                    "the router never served again after the failed restart"
                );
                drop(connection);
                std::thread::sleep(POLL_INTERVAL_DURATION);
            }
            lookup_result => {
                assert_eq!(lookup_result, RouterResult::Found(created_session.clone()));
                break;
            }
        }
    }

    // The program file written again after the failed exec has an access time
    // at least 29 days ahead of the clock.
    let program_file_access_time =
        std::fs::metadata(resolve_router_program_file_path(runtime_directory.path()))
            .and_then(|program_file_metadata| program_file_metadata.accessed())
            .expect("read the program file's access time");
    assert!(
        program_file_access_time
            >= std::time::SystemTime::now() + common::EARLIEST_ACCESS_TIME_LEAD_DURATION,
        "the program file has access time {program_file_access_time:?}"
    );

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
    let mut router_process = start_router_from_binary(
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

    let restarted_endpoint = wait_for_restarted_router_endpoint(
        runtime_directory.path(),
        &endpoint_before_restart,
        &mut router_process,
    );
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
    let mut router_process = start_router_from_binary(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
    );
    let mut connection = connect_to_router(runtime_directory.path());

    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
    };

    let initial_endpoint =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the router advertises its socket");
    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let first_restart_endpoint = wait_for_restarted_router_endpoint(
        runtime_directory.path(),
        &initial_endpoint,
        &mut router_process,
    );
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

    let second_restart_endpoint = wait_for_restarted_router_endpoint(
        runtime_directory.path(),
        &first_restart_endpoint,
        &mut router_process,
    );
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

/// The image a restart starts reads `koshi.kdl` again and takes its log
/// settings from that read. The router starts with logging off and a remote
/// listen address whose remote access was never switched on, and creates no log
/// directory. `koshi.kdl` then turns logging on at `info` in `json`, and the
/// restarted router writes its remote listen note to `koshi-log-router.log` as
/// the one line of that file.
///
/// Each read of the log directory follows a lookup. The dispatcher answers a
/// lookup, and starts after the router writes the remote listen note.
#[test]
fn a_restarted_router_takes_its_log_settings_from_koshi_kdl_again() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    write_test_config(
        test_home_directory.path(),
        "version 2\nremote-listen \"127.0.0.1:7654\"\n",
    );
    let mut router_process = start_router_from_binary(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
    );
    let mut connection = connect_to_router(runtime_directory.path());
    let missing_session_id = SessionId::new();
    assert_eq!(
        send_attach_lookup(
            &mut connection,
            &SessionSelector::SessionId(missing_session_id)
        ),
        build_no_such_session_result(missing_session_id)
    );
    let log_directory = resolve_log_directory_under_home(test_home_directory.path());
    assert!(
        !log_directory.exists(),
        "a router with logging off created {}",
        log_directory.display()
    );

    write_test_config(
        test_home_directory.path(),
        "version 2\n\
         remote-listen \"127.0.0.1:7654\"\n\
         logging {\n    enabled #true\n    level \"info\"\n    format \"json\"\n}\n",
    );
    let endpoint_before_restart =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the router advertises its socket");
    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);
    let restarted_endpoint = wait_for_restarted_router_endpoint(
        runtime_directory.path(),
        &endpoint_before_restart,
        &mut router_process,
    );
    #[cfg(windows)]
    let _restarted_router_process = RunningProcess {
        process_id: restarted_endpoint.process_id,
    };
    #[cfg(unix)]
    // The restart replaced the running image: the process id stays.
    assert_eq!(
        restarted_endpoint.process_id,
        endpoint_before_restart.process_id
    );
    let mut connection = connect_to_router(runtime_directory.path());
    assert_eq!(
        send_attach_lookup(
            &mut connection,
            &SessionSelector::SessionId(missing_session_id)
        ),
        build_no_such_session_result(missing_session_id)
    );

    let router_log_path = log_directory.join("koshi-log-router.log");
    let router_log_text = std::fs::read_to_string(&router_log_path)
        .unwrap_or_else(|read_error| panic!("read {}: {read_error}", router_log_path.display()));
    let router_log_lines: Vec<&str> = router_log_text.lines().collect();
    assert_eq!(router_log_lines.len(), 1, "{router_log_text}");
    let mut logged_line: serde_json::Value =
        serde_json::from_str(router_log_lines[0]).expect("the line is one JSON object");
    logged_line
        .as_object_mut()
        .expect("the line is one JSON object")
        .remove("timestamp")
        .expect("the line carries a timestamp");
    assert_eq!(
        logged_line,
        serde_json::json!({
            "level": "INFO",
            "fields": {
                "message": "remote listen address 127.0.0.1:7654 is set, but remote access was \
                            never switched on; run `koshi share open` to switch it on"
            },
            "target": "koshi_daemon::router"
        })
    );
}

/// Unix only: the restart replaces the router's running image with the
/// program file. The program file is replaced by a shell script that writes
/// `restarted image ends` on its error stream and exits with code `3`, so the
/// image the restart starts ends before it advertises anything. A child shell
/// writes the script and closes it before the router runs it.
#[cfg(unix)]
#[test]
fn the_restart_wait_names_the_exit_status_and_error_lines_of_a_router_that_ended() {
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let binary_path = copy_koshi_binary(runtime_directory.path());
    let mut router_process = start_router_from_binary(
        &binary_path,
        test_home_directory.path(),
        runtime_directory.path(),
    );
    let mut connection = connect_to_router(runtime_directory.path());
    let endpoint_before_restart =
        EndpointFile::load_from_path(&resolve_router_endpoint_path(runtime_directory.path()))
            .expect("the router advertises its socket");

    std::fs::remove_file(&binary_path).expect("the binary is taken away");
    let script_write_status = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(
            "printf '%s\\n' '#!/bin/sh' 'echo restarted image ends >&2' 'exit 3' > \"$1\" \
             && chmod 755 \"$1\"",
        )
        .arg("sh")
        .arg(&binary_path)
        .status()
        .expect("the shell that writes the script runs");
    assert!(script_write_status.success(), "{script_write_status}");

    assert_eq!(
        send_router_request(&mut connection, RouterRequestKind::Restart),
        RouterResult::Restarting
    );
    drop(connection);

    let wait_panic_payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for_restarted_router_endpoint(
            runtime_directory.path(),
            &endpoint_before_restart,
            &mut router_process,
        )
    }))
    .expect_err("no restarted router advertises itself");
    assert_eq!(
        wait_panic_payload
            .downcast_ref::<String>()
            .map(String::as_str),
        Some(
            "the router exited with exit status: 3 before it advertised itself after the \
             restart; its error stream ended with:\nrestarted image ends"
        )
    );
}

#[cfg(unix)]
#[test]
fn a_running_router_and_session_lock_their_runtime_folder_and_set_access_times_ahead() {
    use std::fs::{File, TryLockError};
    use std::time::SystemTime;

    use koshi_ipc::router::compute_router_socket_address;

    let read_access_time = |timed_path: &Path| {
        std::fs::symlink_metadata(timed_path)
            .and_then(|timed_path_metadata| timed_path_metadata.accessed())
            .unwrap_or_else(|read_error| {
                panic!(
                    "read the access time of {}: {read_error}",
                    timed_path.display()
                )
            })
    };
    let test_home_directory = build_short_test_directory();
    let runtime_directory = build_test_runtime_directory();
    let _router_process =
        start_router_process(test_home_directory.path(), runtime_directory.path());
    let mut connection = connect_to_router(runtime_directory.path());
    let created_session = create_session(&mut connection);
    let _session_processes = RunningSessions {
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_addresses: vec![created_session.clone()],
    };

    // Each server set its access times 30 days after its own start, and both
    // started less than a day before `checked_at`.
    let checked_at = SystemTime::now();
    let earliest_access_time = checked_at + common::EARLIEST_ACCESS_TIME_LEAD_DURATION;
    let latest_access_time = checked_at + Duration::from_secs(30 * 24 * 60 * 60);
    let router_file_paths = [
        resolve_router_endpoint_path(runtime_directory.path()),
        resolve_router_program_file_path(runtime_directory.path()),
        Path::new(&compute_router_socket_address(runtime_directory.path())).to_path_buf(),
        resolve_router_lock_path(runtime_directory.path()),
    ];
    for router_file_path in &router_file_paths {
        let access_time = read_access_time(router_file_path);
        assert!(
            (earliest_access_time..=latest_access_time).contains(&access_time),
            "{} has access time {access_time:?}",
            router_file_path.display()
        );
    }
    let session_file_paths = [
        EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            created_session.session_id,
        ),
        ServerProgramFile::resolve_session_program_file_path(
            runtime_directory.path(),
            created_session.session_id,
        ),
        Path::new(&created_session.socket_address).to_path_buf(),
    ];
    // The session starts its keeper right after it prints its ready line, and
    // the keeper's first pass sets the access time of one file after another.
    let wait_deadline = Instant::now() + WAIT_DURATION;
    for session_file_path in &session_file_paths {
        while read_access_time(session_file_path) < earliest_access_time {
            assert!(
                Instant::now() < wait_deadline,
                "the session never set the access time of {} ahead",
                session_file_path.display()
            );
            std::thread::sleep(POLL_INTERVAL_DURATION);
        }
    }
    for session_file_path in &session_file_paths {
        let access_time = read_access_time(session_file_path);
        assert!(
            (earliest_access_time..=latest_access_time).contains(&access_time),
            "{} has access time {access_time:?}",
            session_file_path.display()
        );
    }

    let runtime_folder = File::open(runtime_directory.path()).expect("open the runtime folder");
    assert!(matches!(
        runtime_folder.try_lock(),
        Err(TryLockError::WouldBlock)
    ));
}
