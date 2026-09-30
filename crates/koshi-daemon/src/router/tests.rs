//! Tests for the router's session list, its dispatcher loop, and its remote
//! access.
//!
//! Most run in process against a hand-built list. No real router is bound and
//! no real session server is started. The tests cover the name walk, selector
//! resolution, removal, the idle-exit rule, the lock handover, the answer to a
//! restart request, the report a session server prints, and the three remote
//! access token requests. The integration tests start a real router and a
//! real session server.
//!
//! Where the piece under test reads something real, a real thing stands in
//! for it: a bound listener for a session the router probes or asks to
//! describe itself, and a `/bin/sh` child for a process the router waits on
//! or kills.
//!
//! The remote access tests open the real TLS listener on a loopback port,
//! dial it with the real client, and stand one socket in for the session
//! behind the bridge. The connection a revoke ends is a real one, admitted by
//! a real secret.

use super::*;

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use koshi_core::discovery::{SessionDiscovery, SessionOverview};
use koshi_ipc::endpoint::resolve_advertisement_marker_path;
use koshi_ipc::endpoint::RESTART_WINDOW_DURATION;
use koshi_ipc::protocol::{
    IncomingResponse, IpcRequest, IpcResponse, IpcResult, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use koshi_ipc::remote_tokens::{
    hash_connection_token, TokenEntry, TokenRecord, TOKEN_STORE_FORMAT,
};
use koshi_ipc::remote_wire::{
    self, RemoteClientFrame, RemoteServerFrame, MIN_REMOTE_PROTOCOL_VERSION,
    REMOTE_PROTOCOL_VERSION, REMOTE_REFUSED,
};
use koshi_ipc::router::RouterRequest;
use koshi_link::remote_client::{self, DIAL_TIMEOUT_DURATION};
use koshi_test_support::fixtures::build_test_runtime_directory;

#[test]
fn unreadable_existing_certificate_is_kept_for_recovery() {
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let certificate_path = CertFile::resolve_certificate_file_path(&data_directory);
    std::fs::create_dir_all(certificate_path.parent().expect("certificate parent"))
        .expect("create remote directory");
    std::fs::write(&certificate_path, b"unreadable certificate")
        .expect("write unreadable certificate");

    let certificate_error = load_or_create_certificate(&data_directory)
        .expect_err("the existing certificate must not be replaced");
    match certificate_error {
        IpcError::RemoteFileUnreadable {
            remote_file,
            remote_file_path,
            error_detail,
        } => {
            assert_eq!(remote_file, RemoteFile::Certificate);
            assert_eq!(remote_file_path, certificate_path.display().to_string());
            assert_eq!(error_detail, "expected value at line 1 column 1");
        }
        unexpected_error => panic!("expected an unreadable file, got {unexpected_error:?}"),
    }
    assert_eq!(
        std::fs::read(&certificate_path).expect("read certificate after refusal"),
        b"unreadable certificate"
    );
}

/// Build a session registry from `(session_id, session_name)` pairs.
fn build_session_registry(session_entries: &[(SessionId, &str)]) -> SessionRegistry {
    session_entries
        .iter()
        .map(|(session_id, session_name)| {
            (
                *session_id,
                SessionRecord {
                    session_name: (*session_name).to_string(),
                    socket_address: compute_socket_address(Path::new("/nowhere"), *session_id),
                    process_id: 4242,
                },
            )
        })
        .collect()
}

#[test]
fn the_name_walk_rejects_a_name_the_list_already_holds() {
    // A name the list holds reads as taken. A prefix of it, a longer name, and
    // another name do not.
    let taken_session_id = SessionId::new();
    let session_registry = build_session_registry(&[(taken_session_id, "S-quiet-lake")]);

    assert!(is_session_name_taken(&session_registry, "S-quiet-lake"));
    assert!(!is_session_name_taken(&session_registry, "S-loud-river"));
    assert!(!is_session_name_taken(&session_registry, "S-quiet-lak"));
    assert!(!is_session_name_taken(&session_registry, "S-quiet-lakes"));
}

#[test]
fn the_name_walk_over_an_empty_list_takes_the_first_name_it_tries() {
    let session_registry = SessionRegistry::new();
    let session_name = generate_name(NameKind::Session, |candidate_session_name| {
        is_session_name_taken(&session_registry, candidate_session_name)
    });

    assert_eq!(session_name.split('-').next(), Some("S"));
    assert!(!is_session_name_taken(&session_registry, &session_name));
}

#[test]
fn the_name_walk_hands_back_a_name_the_list_does_not_hold() {
    // With one name taken, a second walk returns a different name.
    let first_session_id = SessionId::new();
    let mut session_registry = SessionRegistry::new();
    let taken_session_name = generate_name(NameKind::Session, |candidate_session_name| {
        is_session_name_taken(&session_registry, candidate_session_name)
    });
    session_registry = build_session_registry(&[(first_session_id, &taken_session_name)]);

    let second_session_name = generate_name(NameKind::Session, |candidate_session_name| {
        is_session_name_taken(&session_registry, candidate_session_name)
    });

    assert_ne!(second_session_name, taken_session_name);
    assert!(!is_session_name_taken(
        &session_registry,
        &second_session_name
    ));
}

#[test]
fn a_selector_resolves_by_id() {
    let requested_session_id = SessionId::new();
    let other_session_id = SessionId::new();
    let absent_session_id = SessionId::new();
    let session_registry = build_session_registry(&[
        (requested_session_id, "S-quiet-lake"),
        (other_session_id, "S-loud-river"),
    ]);

    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionId(requested_session_id)
        ),
        Some(requested_session_id)
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionId(other_session_id)
        ),
        Some(other_session_id)
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionId(absent_session_id)
        ),
        None
    );
}

#[test]
fn a_selector_resolves_by_the_whole_name_only() {
    // `S-quiet` is a prefix of `S-quiet-lake` and resolves to nothing.
    let requested_session_id = SessionId::new();
    let session_registry = build_session_registry(&[
        (requested_session_id, "S-quiet-lake"),
        (SessionId::new(), "S-loud-river"),
    ]);

    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName("S-quiet-lake".to_string())
        ),
        Some(requested_session_id)
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName("S-quiet".to_string())
        ),
        None
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName("s-quiet-lake".to_string())
        ),
        None
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName(String::new())
        ),
        None
    );
}

#[test]
fn removing_one_session_leaves_every_other_entry_in_place() {
    let removed_session_id = SessionId::new();
    let retained_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut session_registry = build_session_registry(&[
        (removed_session_id, "S-quiet-lake"),
        (retained_session_id, "S-loud-river"),
    ]);

    remove_session_from_registry(
        runtime_directory.path(),
        &mut session_registry,
        removed_session_id,
    );

    assert_eq!(
        session_registry,
        build_session_registry(&[(retained_session_id, "S-loud-river")]),
        "only the session that exited leaves the list"
    );
}

#[test]
fn removing_a_session_takes_the_files_it_advertised_with_it() {
    // Removing a session removes its endpoint file and, on Unix, its socket
    // file.
    let removed_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), removed_session_id);
    EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), removed_session_id),
        connection_token: ConnectionToken::from_secret("a".repeat(64)),
        process_id: 4242,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");
    #[cfg(unix)]
    let socket_path = PathBuf::from(compute_socket_address(
        runtime_directory.path(),
        removed_session_id,
    ));
    #[cfg(unix)]
    std::fs::write(&socket_path, b"").expect("the leftover socket file is created");

    let mut session_registry = build_session_registry(&[(removed_session_id, "S-quiet-lake")]);
    remove_session_from_registry(
        runtime_directory.path(),
        &mut session_registry,
        removed_session_id,
    );

    assert_eq!(session_registry, SessionRegistry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
    #[cfg(unix)]
    assert!(!socket_path.exists(), "the socket file is removed");
}

#[test]
fn removing_a_session_that_is_not_in_the_list_still_clears_its_files() {
    // A session server adopted from an earlier router has files but was
    // dropped from the list by an earlier probe.
    let removed_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), removed_session_id);
    EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), removed_session_id),
        connection_token: ConnectionToken::from_secret("b".repeat(64)),
        process_id: 4242,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");

    let mut session_registry = SessionRegistry::new();
    remove_session_from_registry(
        runtime_directory.path(),
        &mut session_registry,
        removed_session_id,
    );

    assert_eq!(session_registry, SessionRegistry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

#[test]
fn the_rebuild_over_an_empty_runtime_directory_finds_no_session() {
    let runtime_directory = build_test_runtime_directory();

    assert_eq!(
        rebuild_session_registry(runtime_directory.path(), None),
        SessionRegistry::new()
    );
}

#[test]
fn the_rebuild_drops_an_endpoint_nothing_listens_behind() {
    // The endpoint file outlived its session server. The rebuild removes it
    // and registers nothing.
    let dead_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), dead_session_id);
    EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), dead_session_id),
        connection_token: ConnectionToken::from_secret("c".repeat(64)),
        process_id: 4242,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");

    assert_eq!(
        rebuild_session_registry(runtime_directory.path(), None),
        SessionRegistry::new()
    );
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

/// Write an endpoint file for `session_id` in `runtime_directory` with the
/// field names `socket`, `token`, and `pid`, which this build does not read.
fn write_unreadable_endpoint_file(runtime_directory: &Path, session_id: SessionId) -> PathBuf {
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    let endpoint_json = serde_json::json!({
        "socket": compute_socket_address(runtime_directory, session_id),
        "token": "f".repeat(64),
        "pid": 5000,
    });
    std::fs::write(&endpoint_path, endpoint_json.to_string())
        .expect("the endpoint file is written");
    endpoint_path
}

#[test]
fn the_rebuild_keeps_the_files_of_a_listening_session_whose_endpoint_file_it_cannot_read() {
    let live_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path = write_unreadable_endpoint_file(runtime_directory.path(), live_session_id);
    let listener = bind_test_session_listener(&compute_socket_address(
        runtime_directory.path(),
        live_session_id,
    ));

    let session_registry = rebuild_session_registry(runtime_directory.path(), None);

    assert_eq!(session_registry, SessionRegistry::new());
    assert!(
        endpoint_path.exists(),
        "a session that is still bound keeps its endpoint file"
    );
    drop(listener);
}

#[test]
fn the_rebuild_removes_an_unreadable_endpoint_file_nothing_listens_behind() {
    let dead_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path = write_unreadable_endpoint_file(runtime_directory.path(), dead_session_id);

    let session_registry = rebuild_session_registry(runtime_directory.path(), None);

    assert_eq!(session_registry, SessionRegistry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

/// Write an empty resume file for `session_id` in `runtime_directory`, stamped
/// `age_duration` old, and hand back its path.
fn build_aged_resume_file(
    runtime_directory: &Path,
    session_id: SessionId,
    age_duration: Duration,
) -> PathBuf {
    let resume_file_path = resolve_resume_file_path(runtime_directory, session_id);
    let resume_file = std::fs::File::create(&resume_file_path).expect("the resume file is written");
    resume_file
        .set_modified(SystemTime::now() - age_duration)
        .expect("the resume file is aged");
    resume_file_path
}

/// One second past [`RESTART_WINDOW_DURATION`]: the swap that wrote a file
/// this old is dead.
const PAST_RESTART_WINDOW_DURATION: Duration =
    Duration::from_secs(RESTART_WINDOW_DURATION.as_secs() + 1);

/// One second, well inside [`RESTART_WINDOW_DURATION`]: the swap that wrote a
/// file this old may still be in flight.
const INSIDE_RESTART_WINDOW_DURATION: Duration = Duration::from_secs(1);

#[test]
fn removing_a_session_takes_its_resume_file_with_it() {
    // A resume file past the window goes with the session's other files.
    let gone_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), gone_session_id);
    EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), gone_session_id),
        connection_token: ConnectionToken::from_secret("d".repeat(64)),
        process_id: 4242,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        gone_session_id,
        PAST_RESTART_WINDOW_DURATION,
    );

    let mut session_registry = build_session_registry(&[(gone_session_id, "S-quiet-lake")]);
    remove_session_from_registry(
        runtime_directory.path(),
        &mut session_registry,
        gone_session_id,
    );

    assert_eq!(session_registry, SessionRegistry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
    assert!(!resume_file_path.exists(), "the resume file is removed");
}

#[test]
fn removing_a_session_that_is_replacing_its_image_leaves_it_and_its_files_alone() {
    // A resume file younger than the window marks a swap in flight. The
    // session keeps its place in the list and every file it advertised with:
    // its new image rebinds the socket and rewrites the endpoint file.
    let replacing_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), replacing_session_id);
    EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), replacing_session_id),
        connection_token: ConnectionToken::from_secret("e".repeat(64)),
        process_id: 4242,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        replacing_session_id,
        INSIDE_RESTART_WINDOW_DURATION,
    );

    let mut session_registry = build_session_registry(&[(replacing_session_id, "S-quiet-lake")]);
    remove_session_from_registry(
        runtime_directory.path(),
        &mut session_registry,
        replacing_session_id,
    );

    assert_eq!(
        session_registry,
        build_session_registry(&[(replacing_session_id, "S-quiet-lake")]),
        "the session stays in the list across the swap"
    );
    assert!(endpoint_path.exists(), "the endpoint file is left in place");
    assert!(
        resume_file_path.exists(),
        "the resume file is left in place"
    );
}

#[test]
fn the_rebuild_removes_a_resume_file_no_session_claims() {
    // A resume file older than the window, with no endpoint file beside it.
    let dead_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        dead_session_id,
        PAST_RESTART_WINDOW_DURATION,
    );

    assert_eq!(
        rebuild_session_registry(runtime_directory.path(), None),
        SessionRegistry::new()
    );
    assert!(
        !resume_file_path.exists(),
        "the orphan resume file is removed"
    );
}

#[test]
fn the_rebuild_leaves_a_resume_file_a_swap_is_still_writing_its_way_out_of() {
    // The session server has written the file and has not yet bound its new
    // socket. The rebuild keeps the file.
    let swapping_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        swapping_session_id,
        INSIDE_RESTART_WINDOW_DURATION,
    );

    assert_eq!(
        rebuild_session_registry(runtime_directory.path(), None),
        SessionRegistry::new()
    );
    assert!(
        resume_file_path.exists(),
        "a swap in flight keeps its resume file"
    );
}

#[test]
fn the_rebuild_leaves_the_resume_file_of_a_session_that_is_still_running() {
    // The file is past the window, and its session is in the list: the file
    // stays.
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        running_session_id,
        PAST_RESTART_WINDOW_DURATION,
    );
    let session_registry = build_session_registry(&[(running_session_id, "S-quiet-lake")]);

    remove_orphan_resume_files(runtime_directory.path(), &session_registry);

    assert!(
        resume_file_path.exists(),
        "a running session keeps its resume file"
    );
}

/// Advertise `session_id` in `shared_sessions_directory` the way a session another local user
/// started advertises itself, and hand back the control-socket address that
/// names. On Unix that is a subdirectory named after another user's id; on
/// Windows it is a marker file beside the ones this user writes.
fn advertise_foreign_session(
    shared_sessions_directory: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let own_user_id = std::fs::metadata(runtime_directory)
            .expect("read the runtime directory")
            .uid();
        let other_user_directory = shared_sessions_directory.join((own_user_id + 1).to_string());
        std::fs::create_dir_all(&other_user_directory).expect("create the other user's directory");
        compute_socket_address(&other_user_directory, session_id)
    }
    #[cfg(windows)]
    {
        let _ = runtime_directory;
        std::fs::create_dir_all(shared_sessions_directory).expect("create the shared directory");
        std::fs::write(
            resolve_advertisement_marker_path(shared_sessions_directory, session_id),
            b"",
        )
        .expect("plant the marker");
        compute_socket_address(shared_sessions_directory, session_id)
    }
}

/// A stand-in session server serving one discovery exchange at
/// `socket_address`: accept one caller, answer the Hello whatever it presents,
/// and describe a session named `session_name` created at
/// `session_created_at`.
///
/// The thread hands back the listener, still bound: a connect probe after the
/// exchange reaches the address until the caller drops it.
fn spawn_session_server_answering_one_connection(
    socket_address: &str,
    session_id: SessionId,
    session_name: &str,
    session_created_at: SystemTime,
) -> JoinHandle<Listener> {
    spawn_session_server_answering_connections(
        socket_address,
        session_id,
        session_name,
        session_created_at,
        1,
    )
}

/// A stand-in session server bound at `socket_address`. On each of
/// `answered_connection_count` connections it answers the Hello and one
/// overview request that names `session_id`, `session_name` and
/// `session_created_at`. The thread then hands back the listener, still bound.
fn spawn_session_server_answering_connections(
    socket_address: &str,
    session_id: SessionId,
    session_name: &str,
    session_created_at: SystemTime,
    answered_connection_count: usize,
) -> JoinHandle<Listener> {
    let listener = Listener::bind(socket_address).expect("bind the stand-in session");
    let session_overview = SessionOverview {
        session: SessionDiscovery {
            session_id,
            session_name: session_name.to_string(),
            created_at: session_created_at,
            attached_client_ids: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    };
    std::thread::spawn(move || {
        for _ in 0..answered_connection_count {
            let mut connection = listener.accept().expect("accept the router");
            let hello_request: IpcRequest = connection.recv().expect("read hello");
            let overview_request: IpcRequest = connection.recv().expect("read discovery request");
            let discovery_responses = [
                IpcResponse {
                    request_id: Some(hello_request.request_id),
                    answer_result: IpcResult::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        build_version: env!("CARGO_PKG_VERSION").to_string(),
                    },
                },
                IpcResponse {
                    request_id: Some(overview_request.request_id),
                    answer_result: IpcResult::Overview(session_overview.clone()),
                },
            ];
            for discovery_response in discovery_responses {
                connection
                    .send(&discovery_response)
                    .expect("send the scripted reply");
            }
        }
        listener
    })
}

/// Write the endpoint file of `session_id` into `runtime_directory`, naming
/// `socket_address` and process id `4242`.
fn write_test_endpoint_file(runtime_directory: &Path, session_id: SessionId, socket_address: &str) {
    EndpointFile {
        socket_address: socket_address.to_string(),
        connection_token: ConnectionToken::from_secret("f".repeat(64)),
        process_id: 4242,
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("the endpoint file is written");
}

#[test]
fn the_rebuild_registers_a_session_another_local_user_started() {
    // The router lists the other user's session with its address and process
    // id `0`.
    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_directory = build_test_runtime_directory();
    let foreign_session_id = SessionId::new();
    let session_created_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let foreign_socket_address = advertise_foreign_session(
        shared_sessions_directory.path(),
        runtime_directory.path(),
        foreign_session_id,
    );
    let session_server_thread = spawn_session_server_answering_one_connection(
        &foreign_socket_address,
        foreign_session_id,
        "S-quiet-lake",
        session_created_at,
    );

    let session_registry = rebuild_session_registry(
        runtime_directory.path(),
        Some(shared_sessions_directory.path()),
    );

    assert_eq!(
        session_registry,
        SessionRegistry::from([(
            foreign_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address: foreign_socket_address,
                process_id: 0,
            },
        )]),
    );

    session_server_thread
        .join()
        .expect("the other user's session exits");
}

#[test]
fn the_rebuild_leaves_out_a_shared_advert_nothing_listens_behind() {
    // The other user's session crashed and left its advert behind. The
    // rebuild skips it and removes none of that user's files.
    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_directory = build_test_runtime_directory();
    let foreign_session_id = SessionId::new();
    let foreign_socket_address = advertise_foreign_session(
        shared_sessions_directory.path(),
        runtime_directory.path(),
        foreign_session_id,
    );
    // On Unix the socket file the session bound outlives it. On Windows the
    // pipe went with the process: only the marker is left.
    let leftover_advert_path = if cfg!(unix) {
        std::fs::write(&foreign_socket_address, b"").expect("plant the leftover socket file");
        PathBuf::from(&foreign_socket_address)
    } else {
        resolve_advertisement_marker_path(shared_sessions_directory.path(), foreign_session_id)
    };

    assert_eq!(
        rebuild_session_registry(
            runtime_directory.path(),
            Some(shared_sessions_directory.path())
        ),
        SessionRegistry::new()
    );
    assert!(
        leftover_advert_path.exists(),
        "the other user's advert is left alone"
    );
}

/// The idle window the dispatcher tests run under: 50 ms.
const TEST_IDLE_EXIT_DURATION: Duration = Duration::from_millis(50);

/// The path a test hands the loop as the binary a restart would start. No
/// test here starts it.
fn get_test_executable_path() -> PathBuf {
    std::env::current_exe().expect("this test binary's own path")
}

/// Remote access as a machine that has none holds it: no listen address, no
/// data directory, no listener, and nothing carried. No test here opens a
/// remote connection.
fn build_no_remote_state() -> RemoteState {
    RemoteState {
        remote_listen_address: None,
        data_directory: None,
        is_listening: false,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    }
}

#[test]
fn an_idle_window_that_passes_with_no_session_running_ends_the_loop() {
    let runtime_directory = build_test_runtime_directory();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let mut session_registry = SessionRegistry::new();

    let router_exit = run_dispatch_loop(
        runtime_directory.path(),
        &get_test_executable_path(),
        None,
        &router_events_sender,
        &router_events_receiver,
        TEST_IDLE_EXIT_DURATION,
        &mut session_registry,
        &mut build_no_remote_state(),
    );

    assert_eq!(router_exit, RouterExit::Idle);
    assert_eq!(session_registry, SessionRegistry::new());
}

#[test]
fn a_request_inside_the_idle_window_is_served_and_the_loop_goes_on() {
    // The loop answers a request queued before it starts, then ends on the
    // idle window.
    let runtime_directory = build_test_runtime_directory();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let (response_sender, response_receiver) = mpsc::channel();
    let mut session_registry = SessionRegistry::new();
    router_events_sender
        .send(RouterEvent::Request {
            request_kind: RouterRequestKind::ListSessions,
            response_sender,
        })
        .expect("the request is queued");

    let router_exit = run_dispatch_loop(
        runtime_directory.path(),
        &get_test_executable_path(),
        None,
        &router_events_sender,
        &router_events_receiver,
        TEST_IDLE_EXIT_DURATION,
        &mut session_registry,
        &mut build_no_remote_state(),
    );

    assert_eq!(
        response_receiver
            .try_recv()
            .expect("the loop answered the request"),
        RouterResult::Sessions(Vec::new())
    );
    assert_eq!(router_exit, RouterExit::Idle);
    assert_eq!(session_registry, SessionRegistry::new());
}

#[test]
fn a_delivered_restart_reply_ends_the_loop_for_the_swap() {
    // `RestartDelivered` ends the loop with `RouterExit::Restart`, and the
    // session list keeps every entry.
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let mut session_registry = build_session_registry(&[(running_session_id, "S-quiet-lake")]);
    router_events_sender
        .send(RouterEvent::RestartDelivered)
        .expect("the delivered reply is queued");

    let router_exit = run_dispatch_loop(
        runtime_directory.path(),
        &get_test_executable_path(),
        None,
        &router_events_sender,
        &router_events_receiver,
        TEST_IDLE_EXIT_DURATION,
        &mut session_registry,
        &mut build_no_remote_state(),
    );

    assert_eq!(router_exit, RouterExit::Restart);
    assert_eq!(
        session_registry,
        build_session_registry(&[(running_session_id, "S-quiet-lake")])
    );
}

#[test]
fn a_running_session_keeps_the_loop_alive_past_the_idle_window() {
    // The loop keeps serving past the idle window while the list holds a
    // session.
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let test_router_event_sender = router_events_sender.clone();
    let held_runtime_directory = runtime_directory.path().to_path_buf();

    let loop_thread = std::thread::spawn(move || {
        let mut session_registry = build_session_registry(&[(running_session_id, "S-quiet-lake")]);
        let router_exit = run_dispatch_loop(
            &held_runtime_directory,
            &get_test_executable_path(),
            None,
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            &mut session_registry,
            &mut build_no_remote_state(),
        );
        (router_exit, session_registry)
    });

    std::thread::sleep(TEST_IDLE_EXIT_DURATION * 5);
    assert!(
        !loop_thread.is_finished(),
        "the loop is still serving while a session is running"
    );

    test_router_event_sender
        .send(RouterEvent::ChildExited(running_session_id))
        .expect("the exit is queued");
    let (router_exit, remaining_session_registry) = loop_thread.join().expect("the loop ended");

    assert_eq!(router_exit, RouterExit::Idle);
    assert_eq!(
        remaining_session_registry,
        SessionRegistry::new(),
        "the session that exited left the list, and the empty list ended the loop"
    );
}

#[test]
fn a_session_that_exits_while_another_runs_leaves_the_loop_serving() {
    let removed_session_id = SessionId::new();
    let retained_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let test_router_event_sender = router_events_sender.clone();
    let held_runtime_directory = runtime_directory.path().to_path_buf();

    let loop_thread = std::thread::spawn(move || {
        let mut session_registry = build_session_registry(&[
            (removed_session_id, "S-quiet-lake"),
            (retained_session_id, "S-loud-river"),
        ]);
        let router_exit = run_dispatch_loop(
            &held_runtime_directory,
            &get_test_executable_path(),
            None,
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            &mut session_registry,
            &mut build_no_remote_state(),
        );
        (router_exit, session_registry)
    });

    test_router_event_sender
        .send(RouterEvent::ChildExited(removed_session_id))
        .expect("the exit is queued");
    std::thread::sleep(TEST_IDLE_EXIT_DURATION * 5);
    assert!(
        !loop_thread.is_finished(),
        "the loop keeps serving while one session is listed"
    );

    test_router_event_sender
        .send(RouterEvent::ChildExited(retained_session_id))
        .expect("the second exit is queued");
    let (router_exit, remaining_session_registry) = loop_thread.join().expect("the loop ended");

    assert_eq!(router_exit, RouterExit::Idle);
    assert_eq!(remaining_session_registry, SessionRegistry::new());
}

#[test]
fn a_lookup_for_a_session_the_list_does_not_hold_is_refused_by_name() {
    let runtime_directory = build_test_runtime_directory();
    let mut session_registry = SessionRegistry::new();

    let router_result = lookup_session_attachment(
        runtime_directory.path(),
        &mut session_registry,
        &SessionSelector::SessionName("S-quiet-lake".to_string()),
    );

    assert_eq!(
        router_result,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: "no session named `S-quiet-lake` is running".to_string(),
        })
    );
}

#[test]
fn a_lookup_finding_nothing_listening_drops_the_session_and_its_files() {
    // The lookup probes the listed address. Nothing answers there: the router
    // removes the session from the list and deletes its endpoint file.
    let dead_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), dead_session_id);
    EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), dead_session_id),
        connection_token: ConnectionToken::from_secret("d".repeat(64)),
        process_id: 4242,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");
    let mut session_registry = build_session_registry(&[(dead_session_id, "S-quiet-lake")]);
    session_registry
        .get_mut(&dead_session_id)
        .expect("the session is listed")
        .socket_address = compute_socket_address(runtime_directory.path(), dead_session_id);

    let router_result = lookup_session_attachment(
        runtime_directory.path(),
        &mut session_registry,
        &SessionSelector::SessionId(dead_session_id),
    );

    assert_eq!(
        router_result,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: format!("no session {dead_session_id} is running"),
        })
    );
    assert_eq!(session_registry, SessionRegistry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

/// A stand-in session server bound at `socket_address`. It accepts one
/// connection and answers the Hello with `PROTOCOL_VERSION + 1`, a version
/// outside the range this build asks for.
fn spawn_version_mismatched_session_server(socket_address: &str) -> JoinHandle<()> {
    let listener = Listener::bind(socket_address).expect("bind the live session");
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the router");
        let hello_request: IpcRequest = connection.recv().expect("read hello");
        let _overview_request: IpcRequest = connection.recv().expect("read discovery request");
        let _ = connection.send(&IpcResponse {
            request_id: Some(hello_request.request_id),
            answer_result: IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION + 1,
                build_version: env!("CARGO_PKG_VERSION").to_string(),
            },
        });
    })
}

#[test]
fn a_listing_keeps_a_session_that_answers_with_a_version_this_build_does_not_read() {
    let live_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), live_session_id);
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), live_session_id);
    EndpointFile {
        socket_address: socket_address.clone(),
        connection_token: ConnectionToken::from_secret("e".repeat(64)),
        process_id: 4242,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");
    let session_server_thread = spawn_version_mismatched_session_server(&socket_address);

    let mut session_registry = build_session_registry(&[(live_session_id, "S-quiet-lake")]);
    session_registry
        .get_mut(&live_session_id)
        .expect("the session is listed")
        .socket_address
        .clone_from(&socket_address);

    let listing_result = list_session_overviews(runtime_directory.path(), &mut session_registry);
    session_server_thread
        .join()
        .expect("the stand-in session ended");

    assert_eq!(
        listing_result,
        RouterResult::Sessions(Vec::new()),
        "a session that could not describe itself is left out of the listing"
    );
    assert_eq!(
        session_registry.keys().copied().collect::<Vec<SessionId>>(),
        vec![live_session_id],
        "a session that is still bound stays in the list"
    );
    assert!(
        endpoint_path.exists(),
        "a session that is still bound keeps its endpoint file"
    );
}

#[test]
fn the_rebuild_keeps_the_files_of_a_session_it_cannot_read_a_version_from() {
    let live_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), live_session_id);
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), live_session_id);
    EndpointFile {
        socket_address: socket_address.clone(),
        connection_token: ConnectionToken::from_secret("f".repeat(64)),
        process_id: 4242,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");
    let session_server_thread = spawn_version_mismatched_session_server(&socket_address);

    let session_registry = rebuild_session_registry(runtime_directory.path(), None);
    session_server_thread
        .join()
        .expect("the stand-in session ended");

    assert_eq!(
        session_registry,
        SessionRegistry::new(),
        "a session that could not describe itself is not listed"
    );
    assert!(
        endpoint_path.exists(),
        "a session that is still bound keeps its endpoint file"
    );
}

/// A stand-in session server bound at `socket_address`. It never calls
/// `accept`: a lookup's probe connects to the bound address and closes at
/// once.
///
/// The caller keeps the returned listener bound for as long as the lookup runs.
fn bind_test_session_listener(socket_address: &str) -> Listener {
    Listener::bind(socket_address).expect("bind the stand-in session")
}

#[test]
fn a_lookup_for_a_session_that_answers_hands_back_where_it_listens() {
    // The lookup probes the address before it hands it out. A session bound at
    // that address is answered with the name, address and process id the list
    // holds for it.
    let live_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), live_session_id);
    let session_listener = bind_test_session_listener(&socket_address);
    let mut session_registry = build_session_registry(&[(live_session_id, "S-quiet-lake")]);
    session_registry
        .get_mut(&live_session_id)
        .expect("the session is listed")
        .socket_address
        .clone_from(&socket_address);

    let router_result = lookup_session_attachment(
        runtime_directory.path(),
        &mut session_registry,
        &SessionSelector::SessionName("S-quiet-lake".to_string()),
    );

    assert_eq!(
        router_result,
        RouterResult::Found(SessionAddress {
            session_id: live_session_id,
            session_name: "S-quiet-lake".to_string(),
            socket_address: socket_address.clone(),
            process_id: 4242,
        })
    );
    assert_eq!(
        session_registry,
        SessionRegistry::from([(
            live_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: 4242,
            },
        )]),
        "a session that answered stays in the list"
    );

    drop(session_listener);
}

#[test]
fn a_lookup_registers_a_session_the_list_did_not_hold() {
    let unlisted_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), unlisted_session_id);
    let session_created_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let session_server_thread = spawn_session_server_answering_one_connection(
        &socket_address,
        unlisted_session_id,
        "S-quiet-lake",
        session_created_at,
    );
    write_test_endpoint_file(
        runtime_directory.path(),
        unlisted_session_id,
        &socket_address,
    );
    let mut session_registry = SessionRegistry::new();

    let lookup_result = lookup_session_attachment(
        runtime_directory.path(),
        &mut session_registry,
        &SessionSelector::SessionName("S-quiet-lake".to_string()),
    );

    assert_eq!(
        lookup_result,
        RouterResult::Found(SessionAddress {
            session_id: unlisted_session_id,
            session_name: "S-quiet-lake".to_string(),
            socket_address: socket_address.clone(),
            process_id: 4242,
        })
    );
    let listener = session_server_thread
        .join()
        .expect("the stand-in session ended");
    assert_eq!(
        session_registry,
        SessionRegistry::from([(
            unlisted_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: 4242,
            },
        )])
    );
    drop(listener);
}

#[test]
fn a_listing_registers_and_lists_a_session_the_list_did_not_hold() {
    let unlisted_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), unlisted_session_id);
    let session_created_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let session_server_thread = spawn_session_server_answering_connections(
        &socket_address,
        unlisted_session_id,
        "S-quiet-lake",
        session_created_at,
        2,
    );
    write_test_endpoint_file(
        runtime_directory.path(),
        unlisted_session_id,
        &socket_address,
    );
    let mut session_registry = SessionRegistry::new();

    let listing_result = list_session_overviews(runtime_directory.path(), &mut session_registry);

    assert_eq!(
        listing_result,
        RouterResult::Sessions(vec![SessionDiscovery {
            session_id: unlisted_session_id,
            session_name: "S-quiet-lake".to_string(),
            created_at: session_created_at,
            attached_client_ids: Vec::new(),
            pane_count: 0,
        }])
    );
    let listener = session_server_thread
        .join()
        .expect("the stand-in session ended");
    assert_eq!(
        session_registry.keys().copied().collect::<Vec<SessionId>>(),
        vec![unlisted_session_id]
    );
    drop(listener);
}

#[test]
fn a_remote_row_request_registers_a_session_the_list_did_not_hold() {
    let unlisted_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), unlisted_session_id);
    let session_created_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let session_server_thread = spawn_session_server_answering_one_connection(
        &socket_address,
        unlisted_session_id,
        "S-quiet-lake",
        session_created_at,
    );
    write_test_endpoint_file(
        runtime_directory.path(),
        unlisted_session_id,
        &socket_address,
    );
    let mut session_registry = SessionRegistry::new();
    let (rows_sender, rows_receiver) = mpsc::channel();

    serve_remote_admission(
        runtime_directory.path(),
        None,
        &mut session_registry,
        &mut build_no_remote_state(),
        AdmissionAsk::ListRows {
            scope: TokenScope::HostWide,
            response_sender: rows_sender,
        },
    );

    assert_eq!(
        rows_receiver.recv().expect("the rows are answered"),
        vec![RemoteSessionRow {
            session_id: unlisted_session_id,
            session_name: "S-quiet-lake".to_string(),
        }]
    );
    let listener = session_server_thread
        .join()
        .expect("the stand-in session ended");
    drop(listener);
}

#[test]
fn a_remote_locate_request_registers_a_session_the_list_did_not_hold() {
    let unlisted_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), unlisted_session_id);
    let session_created_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let session_server_thread = spawn_session_server_answering_one_connection(
        &socket_address,
        unlisted_session_id,
        "S-quiet-lake",
        session_created_at,
    );
    write_test_endpoint_file(
        runtime_directory.path(),
        unlisted_session_id,
        &socket_address,
    );
    let mut session_registry = SessionRegistry::new();
    let mut remote_state = build_no_remote_state();
    let (admitted_stream, _caller_stream) = build_loopback_connection_pair();
    remote_state
        .admitted_remote_connections
        .push(AdmittedRemoteConnection {
            token_hash: "g".repeat(64),
            tcp_stream: admitted_stream,
            remote_connection_id: 0,
        });
    let (endpoint_sender, endpoint_receiver) = mpsc::channel();

    serve_remote_admission(
        runtime_directory.path(),
        None,
        &mut session_registry,
        &mut remote_state,
        AdmissionAsk::Locate {
            scope: TokenScope::HostWide,
            remote_connection_id: 0,
            session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
            response_sender: endpoint_sender,
        },
    );

    assert_eq!(
        endpoint_receiver.recv().expect("the location is answered"),
        Some(EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            unlisted_session_id
        ))
    );
    let listener = session_server_thread
        .join()
        .expect("the stand-in session ended");
    drop(listener);
}

#[test]
fn a_listing_answers_the_sessions_that_describe_themselves_in_name_then_id_order() {
    // The listing sorts the sessions by name, then by id.
    let runtime_directory = build_test_runtime_directory();
    let loud_river_session_id = SessionId::new();
    let quiet_lake_session_id = SessionId::new();
    let session_created_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut session_server_threads = Vec::new();
    for (session_id, session_name) in [
        (loud_river_session_id, "S-loud-river"),
        (quiet_lake_session_id, "S-quiet-lake"),
    ] {
        let socket_address = compute_socket_address(runtime_directory.path(), session_id);
        session_server_threads.push(spawn_session_server_answering_one_connection(
            &socket_address,
            session_id,
            session_name,
            session_created_at,
        ));
        EndpointFile {
            socket_address,
            connection_token: ConnectionToken::from_secret("f".repeat(64)),
            process_id: 4242,
        }
        .write_to_path(&EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            session_id,
        ))
        .expect("the endpoint file is written");
    }
    let mut session_registry = build_session_registry(&[
        (quiet_lake_session_id, "S-quiet-lake"),
        (loud_river_session_id, "S-loud-river"),
    ]);

    let listing_result = list_session_overviews(runtime_directory.path(), &mut session_registry);

    assert_eq!(
        listing_result,
        RouterResult::Sessions(vec![
            SessionDiscovery {
                session_id: loud_river_session_id,
                session_name: "S-loud-river".to_string(),
                created_at: session_created_at,
                attached_client_ids: Vec::new(),
                pane_count: 0,
            },
            SessionDiscovery {
                session_id: quiet_lake_session_id,
                session_name: "S-quiet-lake".to_string(),
                created_at: session_created_at,
                attached_client_ids: Vec::new(),
                pane_count: 0,
            },
        ])
    );
    assert_eq!(
        session_registry,
        build_session_registry(&[
            (quiet_lake_session_id, "S-quiet-lake"),
            (loud_river_session_id, "S-loud-river"),
        ]),
        "both sessions answered and stay in the list"
    );

    for session_server_thread in session_server_threads {
        session_server_thread
            .join()
            .expect("the stand-in session ended");
    }
}

#[test]
fn a_listing_drops_every_session_that_does_not_answer() {
    let dead_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut session_registry = build_session_registry(&[(dead_session_id, "S-quiet-lake")]);

    let listing_result = list_session_overviews(runtime_directory.path(), &mut session_registry);

    assert_eq!(listing_result, RouterResult::Sessions(Vec::new()));
    assert_eq!(session_registry, SessionRegistry::new());
}

#[test]
fn the_session_server_starts_in_the_directory_the_request_named() {
    // The session server command runs in the working directory the create
    // request named.
    let runtime_directory = build_test_runtime_directory();
    let working_directory = build_test_runtime_directory();

    let session_server_command = build_session_server_command(
        runtime_directory.path(),
        SessionId::new(),
        "S-quiet-lake",
        None,
        Some(working_directory.path()),
        None,
    )
    .expect("the command is built");

    assert_eq!(
        session_server_command.get_current_dir(),
        Some(working_directory.path())
    );
}

/// The arguments a session server is started with, in order, as plain strings.
fn list_command_arguments(session_server_command: &std::process::Command) -> Vec<String> {
    session_server_command
        .get_args()
        .map(|command_argument| command_argument.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn a_create_that_asked_for_no_other_users_starts_the_session_without_the_flag() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();

    let session_server_command = build_session_server_command(
        runtime_directory.path(),
        session_id,
        "S-quiet-lake",
        None,
        None,
        None,
    )
    .expect("the command is built");

    assert_eq!(
        list_command_arguments(&session_server_command),
        vec![
            "serve-session".to_string(),
            session_id.to_string(),
            "S-quiet-lake".to_string(),
            "--runtime-dir".to_string(),
            runtime_directory.path().to_string_lossy().into_owned(),
        ]
    );
}

#[test]
fn a_create_that_asked_for_the_other_users_starts_the_session_under_the_flag() {
    // `Some(true)` adds `--allow-other-users` after the profile arguments.
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();

    let session_server_command = build_session_server_command(
        runtime_directory.path(),
        session_id,
        "S-quiet-lake",
        Some("dev"),
        None,
        Some(true),
    )
    .expect("the command is built");

    assert_eq!(
        list_command_arguments(&session_server_command),
        vec![
            "serve-session".to_string(),
            session_id.to_string(),
            "S-quiet-lake".to_string(),
            "--runtime-dir".to_string(),
            runtime_directory.path().to_string_lossy().into_owned(),
            "--profile".to_string(),
            "dev".to_string(),
            "--allow-other-users".to_string(),
        ]
    );
}

#[test]
fn a_create_that_refused_the_other_users_starts_the_session_without_the_flag() {
    // `Some(false)` builds the same arguments as `None`: the session reads the
    // setting from its own `koshi.kdl`.
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();

    let session_server_command = build_session_server_command(
        runtime_directory.path(),
        session_id,
        "S-quiet-lake",
        None,
        None,
        Some(false),
    )
    .expect("the command is built");

    assert_eq!(
        list_command_arguments(&session_server_command),
        vec![
            "serve-session".to_string(),
            session_id.to_string(),
            "S-quiet-lake".to_string(),
            "--runtime-dir".to_string(),
            runtime_directory.path().to_string_lossy().into_owned(),
        ]
    );
}

/// `CREATE_NO_WINDOW` holds the Win32 value `0x0800_0000`. The router starts
/// every session server with this creation flag: the session server runs with
/// no console window.
#[cfg(windows)]
#[test]
fn the_no_window_flag_carries_the_win32_value() {
    assert_eq!(
        CREATE_NO_WINDOW, 0x0800_0000,
        "CREATE_NO_WINDOW is 134217728; another value is another flag"
    );
}

#[test]
fn a_create_that_names_no_directory_leaves_the_child_where_the_router_is() {
    let runtime_directory = build_test_runtime_directory();

    let session_server_command = build_session_server_command(
        runtime_directory.path(),
        SessionId::new(),
        "S-quiet-lake",
        None,
        None,
        None,
    )
    .expect("the command is built");

    assert_eq!(session_server_command.get_current_dir(), None);
}

/// How long a lock-handover test holds the lock before it releases it: 200 ms,
/// inside [`LOCK_HANDOVER_TIMEOUT_DURATION`].
const TEST_LOCK_HOLD_DURATION: Duration = Duration::from_millis(200);

/// One handle on the router lock file in `runtime_directory`, opened with the
/// options [`run_router`] uses.
fn open_router_lock_file(runtime_directory: &Path) -> File {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(resolve_router_lock_path(runtime_directory))
        .expect("the router lock file opens")
}

#[test]
fn a_router_that_does_not_wait_yields_to_the_router_holding_the_lock() {
    let runtime_directory = build_test_runtime_directory();
    let holding_lock_file = open_router_lock_file(runtime_directory.path());
    let arriving_lock_file = open_router_lock_file(runtime_directory.path());

    assert!(
        take_router_lock(&holding_lock_file, false).expect("the first router takes the lock"),
        "an unlocked router lock is taken on the first attempt"
    );
    assert!(
        !take_router_lock(&arriving_lock_file, false).expect("the second router reads the lock"),
        "a held lock sends the arriving router to the one holding it"
    );
}

#[test]
fn a_router_that_waits_takes_the_lock_the_previous_router_releases() {
    // The replacement router asks for the lock while the previous router holds
    // it, and takes it once the previous router drops its handle.
    let runtime_directory = build_test_runtime_directory();
    let previous_lock_file = open_router_lock_file(runtime_directory.path());
    let replacement_lock_file = open_router_lock_file(runtime_directory.path());
    assert!(
        take_router_lock(&previous_lock_file, false).expect("the previous router takes the lock")
    );

    let release_thread = std::thread::spawn(move || {
        std::thread::sleep(TEST_LOCK_HOLD_DURATION);
        drop(previous_lock_file);
    });
    let is_lock_taken =
        take_router_lock(&replacement_lock_file, true).expect("the replacement waits for the lock");
    release_thread
        .join()
        .expect("the previous router shut down");

    assert!(
        is_lock_taken,
        "the replacement takes the lock that was released"
    );
}

#[test]
fn a_restart_request_is_answered_from_the_binary_on_disk() {
    let runtime_directory = build_test_runtime_directory();
    let executable_path = runtime_directory.path().join("koshi");
    std::fs::write(&executable_path, b"").expect("the stand-in binary is written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in binary is executable");
    }
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut session_registry = SessionRegistry::new();

    let router_result = serve_router_request(
        runtime_directory.path(),
        &executable_path,
        None,
        &mut session_registry,
        &mut build_no_remote_state(),
        &router_events_sender,
        RouterRequestKind::Restart,
    );

    assert_eq!(router_result, RouterResult::Restarting);
}

#[test]
fn a_restart_request_naming_a_binary_that_cannot_be_read_is_refused() {
    // A path with nothing at it is refused before the router answers
    // `Restarting`.
    let runtime_directory = build_test_runtime_directory();
    let executable_path = runtime_directory.path().join("koshi");
    let metadata_error = std::fs::metadata(&executable_path).expect_err("nothing is at that path");
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut session_registry = SessionRegistry::new();

    let router_result = serve_router_request(
        runtime_directory.path(),
        &executable_path,
        None,
        &mut session_registry,
        &mut build_no_remote_state(),
        &router_events_sender,
        RouterRequestKind::Restart,
    );

    assert_eq!(
        router_result,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!(
                "the binary at {} could not be read: {metadata_error}",
                executable_path.display()
            ),
        })
    );
}

// A binary with no execute permission is refused before the router answers
// `Restarting`.
#[cfg(unix)]
#[test]
fn a_restart_request_naming_a_non_executable_binary_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let runtime_directory = build_test_runtime_directory();
    let executable_path = runtime_directory.path().join("koshi");
    std::fs::write(&executable_path, b"").expect("the stand-in binary is written");
    std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o644))
        .expect("the execute permission is dropped");
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut session_registry = SessionRegistry::new();

    let router_result = serve_router_request(
        runtime_directory.path(),
        &executable_path,
        None,
        &mut session_registry,
        &mut build_no_remote_state(),
        &router_events_sender,
        RouterRequestKind::Restart,
    );

    assert_eq!(
        router_result,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!(
                "the binary at {} is not executable",
                executable_path.display()
            ),
        })
    );
}

/// A process this test is the parent of, running `shell_script` under
/// `/bin/sh` with its three standard streams set to null. Dropping the handle
/// does not wait on the process; the caller collects the exit.
#[cfg(unix)]
fn spawn_running_child(shell_script: &str) -> Child {
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(shell_script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the shell runs")
}

/// A process this test is the parent of, which ends at once.
#[cfg(unix)]
fn spawn_short_lived_child() -> Child {
    spawn_running_child("exit 0")
}

/// The watcher sends `ChildExited` with the session id once the child it
/// watches exits. The watcher thread holds the only other sender: an event or
/// a closed channel ends the wait.
#[cfg(unix)]
#[test]
fn the_watcher_reports_the_exit_of_a_session_this_process_is_the_parent_of() {
    let session_id = SessionId::new();
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    watch_session_process_exit(
        spawn_short_lived_child().id(),
        session_id,
        router_events_sender,
    );

    match router_events_receiver.recv() {
        Ok(RouterEvent::ChildExited(reported_session_id)) => {
            assert_eq!(reported_session_id, session_id)
        }
        Ok(_) => panic!("the watcher reported something other than the session's exit"),
        Err(mpsc::RecvError) => panic!("the watcher ended without reporting the exit"),
    }
}

/// The reaper waits on the session server the router started and sends
/// `ChildExited` with the session id once it exits. The reaper thread holds the
/// only other sender: an event or a closed channel ends the wait.
#[cfg(unix)]
#[test]
fn the_reaper_reports_the_exit_of_the_session_server_it_started() {
    let session_id = SessionId::new();
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    start_session_reaper_thread(spawn_short_lived_child(), session_id, router_events_sender);

    match router_events_receiver.recv() {
        Ok(RouterEvent::ChildExited(reported_session_id)) => {
            assert_eq!(reported_session_id, session_id)
        }
        Ok(_) => panic!("the reaper reported something other than the session's exit"),
        Err(mpsc::RecvError) => panic!("the reaper ended without reporting the exit"),
    }
}

/// `terminate_child_process` kills the child with `SIGKILL` and waits on it:
/// the child's exit status is collected.
#[cfg(unix)]
#[test]
fn a_child_that_never_became_a_session_is_killed_and_collected() {
    use std::os::unix::process::ExitStatusExt as _;

    let mut child_process = spawn_running_child("sleep 30");

    terminate_child_process(&mut child_process);

    let child_exit_status = child_process
        .try_wait()
        .expect("the child's status reads back")
        .expect("the child's exit status is collected");
    assert_eq!(
        child_exit_status.signal(),
        Some(libc::SIGKILL),
        "the child ends on SIGKILL"
    );
}

/// A session this router is not the parent of — one another user started, or
/// one adopted from a router that exited — fails the wait at once, and the
/// thread ends without reporting. The entry stays until a lookup probes its
/// socket.
#[cfg(unix)]
#[test]
fn the_watcher_over_a_session_this_process_did_not_start_reports_nothing() {
    // The process that started this test is never a child of it.
    let parent_process_id =
        u32::try_from(unsafe { libc::getppid() }).expect("a process id is positive");
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    watch_session_process_exit(parent_process_id, SessionId::new(), router_events_sender);

    assert_eq!(router_events_receiver.recv().err(), Some(mpsc::RecvError));
}

/// The router hands its place over on Windows by starting the new binary with
/// this argument, the one [`crate::cli`] parses into `wait_for_lock`. The two
/// creation flags the handover carries are checked beside them, in
/// [`crate::process`].
#[cfg(windows)]
#[test]
fn the_handover_carries_the_argument_that_waits() {
    assert_eq!(WAIT_FOR_LOCK_FLAG, "--wait-for-lock");
}

/// A restart whose `execvp` fails leaves SIGPIPE at `SIG_IGN`.
///
/// The file the restart names is readable and not executable: `fs::metadata`
/// succeeds and `execvp` fails with `EACCES`. The assertion reads the
/// disposition by installing `SIG_IGN`, the value it expects.
#[cfg(unix)]
#[test]
fn a_restart_that_cannot_exec_leaves_the_write_to_a_hung_up_client_ignored() {
    use std::os::unix::fs::PermissionsExt;

    let runtime_directory = build_test_runtime_directory();
    let executable_path = runtime_directory.path().join("koshi");
    std::fs::write(&executable_path, b"").expect("the stand-in binary is written");
    std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o644))
        .expect("the stand-in binary is readable and not executable");

    let restart_error = restart_by_exec(&executable_path, runtime_directory.path());

    assert_eq!(restart_error.kind(), std::io::ErrorKind::PermissionDenied);
    let prior_sigpipe_disposition = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    assert_eq!(prior_sigpipe_disposition, libc::SIG_IGN);
}

/// The accept loop checks the user the OS reports for each connection and
/// serves a connection the router's own user opened. The caller here runs in
/// the test process, under that same user.
#[test]
fn the_accept_loop_serves_a_connection_this_user_opened() {
    let runtime_directory = build_test_runtime_directory();
    let router_socket_address = compute_router_socket_address(runtime_directory.path());
    let router_listener =
        Listener::bind(&router_socket_address).expect("the router socket is bound");
    let router_connection_token = ConnectionToken::generate();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let is_shutting_down = Arc::new(AtomicBool::new(false));
    let accept_loop_shutdown_flag = Arc::clone(&is_shutting_down);
    let accepted_connection_token = router_connection_token.clone();
    let accept_thread = std::thread::spawn(move || {
        run_router_accept_loop(
            &router_listener,
            &accepted_connection_token,
            &router_events_sender,
            &accept_loop_shutdown_flag,
        );
    });

    let mut caller_connection =
        Connection::connect(&router_socket_address).expect("the caller reaches the router");
    caller_connection
        .send(&RouterRequest {
            request_id: 1,
            request_kind: RouterRequestKind::build_hello_request(router_connection_token),
        })
        .expect("the hello is written");
    let hello_response: RouterResponse = caller_connection.recv().expect("the hello is answered");
    assert_eq!(
        hello_response,
        RouterResponse {
            request_id: Some(1),
            answer_result: RouterResult::Hello {
                protocol_version: ROUTER_PROTOCOL_VERSION,
                build_version: BUILD_VERSION.to_string(),
            },
        }
    );
    caller_connection
        .send(&RouterRequest {
            request_id: 2,
            request_kind: RouterRequestKind::ListSessions,
        })
        .expect("the listing request is written");
    let received_event = router_events_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the request reaches the dispatcher");
    let RouterEvent::Request {
        request_kind,
        response_sender: _,
    } = received_event
    else {
        panic!("the accept loop passed on something other than a request");
    };
    assert_eq!(request_kind, RouterRequestKind::ListSessions);

    is_shutting_down.store(true, Ordering::SeqCst);
    drop(caller_connection);
    let _ = Connection::connect(&router_socket_address);
    accept_thread.join().expect("the accept loop ends");
}

/// Answer one token request against `token_store_path`, with an empty session
/// list, a fresh temporary runtime directory, and an events channel nothing
/// reads. `token_store_path` is `None` for a machine with no data directory.
fn answer_token_request(
    token_store_path: Option<&Path>,
    request_kind: RouterRequestKind,
) -> RouterResult {
    let runtime_directory = build_test_runtime_directory();
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut session_registry = SessionRegistry::new();
    serve_router_request(
        runtime_directory.path(),
        &get_test_executable_path(),
        token_store_path,
        &mut session_registry,
        &mut build_no_remote_state(),
        &router_events_sender,
        request_kind,
    )
}

/// A grant request for `identity` on `scope` that expires after `expires_in`,
/// or never expires when `expires_in` is `None`.
fn build_grant_request(
    identity: &str,
    scope: TokenScope,
    expires_in: Option<Duration>,
) -> RouterRequestKind {
    RouterRequestKind::GrantToken {
        identity: identity.to_string(),
        scope,
        expires_in,
    }
}

/// Grant `identity` a token on `scope` with no expiry and hand back its
/// secret. A refused grant panics.
fn grant_token_for_test(
    token_store_path: &Path,
    identity: &str,
    scope: TokenScope,
) -> ConnectionToken {
    let token_request_result = answer_token_request(
        Some(token_store_path),
        build_grant_request(identity, scope, None),
    );
    match token_request_result {
        RouterResult::Granted {
            connection_token, ..
        } => connection_token,
        unexpected_router_result => panic!("the grant was refused: {unexpected_router_result:?}"),
    }
}

/// Rewrite the token store at `token_store_path` with line breaks and indents,
/// and hand back the bytes now on disk.
///
/// The reader accepts the spaced bytes, and the store writer writes compact
/// JSON: a later byte comparison against the returned bytes fails after any
/// write to the store, even a write of the same records.
fn rewrite_token_store_with_spacing(token_store_path: &Path) -> Vec<u8> {
    let token_store =
        TokenStore::load_token_store_from_path(token_store_path).expect("the store reads back");
    let spaced_token_store_bytes =
        serde_json::to_vec_pretty(&token_store).expect("the store encodes with indents");
    std::fs::write(token_store_path, &spaced_token_store_bytes)
        .expect("the spaced store is written");
    spaced_token_store_bytes
}

/// The `MalformedRequest` error carrying `refusal_message`: the answer every
/// token request gets when the store cannot be opened.
fn build_token_refusal_result(refusal_message: &str) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::MalformedRequest,
        message: refusal_message.to_string(),
    })
}

/// One request of each token kind: a grant, a revoke and a listing.
fn list_token_request_kinds(session_id: SessionId) -> [RouterRequestKind; 3] {
    [
        build_grant_request("ada", TokenScope::HostWide, None),
        RouterRequestKind::RevokeToken {
            identity: "ada".to_string(),
            scope: None,
        },
        RouterRequestKind::ListTokens {
            scope: Some(TokenScope::Session(session_id)),
        },
    ]
}

#[test]
fn a_grant_writes_one_record_holding_the_hash_of_the_secret_it_hands_back() {
    // The grant hands the secret back in its answer. The store file holds only
    // the secret's hash.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(runtime_directory.path());

    let grant_result = answer_token_request(
        Some(&token_store_path),
        build_grant_request("ada", TokenScope::HostWide, None),
    );

    let RouterResult::Granted {
        connection_token,
        has_replaced_active_grant,
    } = grant_result
    else {
        panic!("the grant was refused: {grant_result:?}")
    };
    assert!(
        !has_replaced_active_grant,
        "the store held no grant for ada to replace"
    );
    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    assert_eq!(written_token_store.store_format, TOKEN_STORE_FORMAT);
    assert_eq!(written_token_store.token_records.len(), 1);
    assert_eq!(written_token_store.token_records[0].identity, "ada");
    assert_eq!(
        written_token_store.token_records[0].token_hash,
        hash_connection_token(&connection_token)
    );
    assert_eq!(
        written_token_store.token_records[0].scope,
        TokenScope::HostWide
    );
    assert_eq!(written_token_store.token_records[0].expires_at, None);
    assert_eq!(written_token_store.token_records[0].last_used_at, None);
    assert_eq!(written_token_store.token_records[0].revoked_at, None);
    let token_store_file_bytes =
        std::fs::read(&token_store_path).expect("the store file is on disk");
    assert!(
        !String::from_utf8_lossy(&token_store_file_bytes)
            .contains(connection_token.expose_secret()),
        "the secret itself never reaches the disk"
    );
}

#[test]
fn a_second_grant_replaces_the_one_on_the_same_scope_and_adds_one_on_another() {
    // A second grant for the same identity and scope replaces the first record.
    // A grant on another scope adds a second record.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(runtime_directory.path());
    let granted_session_id = SessionId::new();
    let original_host_wide_token =
        grant_token_for_test(&token_store_path, "ada", TokenScope::HostWide);

    let repeated_grant_result = answer_token_request(
        Some(&token_store_path),
        build_grant_request("ada", TokenScope::HostWide, None),
    );

    let RouterResult::Granted {
        connection_token: replacement_connection_token,
        has_replaced_active_grant,
    } = repeated_grant_result
    else {
        panic!("the second grant was refused: {repeated_grant_result:?}")
    };
    assert!(
        has_replaced_active_grant,
        "ada already held a host-wide grant"
    );
    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    assert_eq!(written_token_store.token_records.len(), 1);
    assert_eq!(
        written_token_store.token_records[0].token_hash,
        hash_connection_token(&replacement_connection_token)
    );
    assert_ne!(
        hash_connection_token(&replacement_connection_token),
        hash_connection_token(&original_host_wide_token),
        "the replacement hands out a different secret"
    );

    let session_grant_result = answer_token_request(
        Some(&token_store_path),
        build_grant_request("ada", TokenScope::Session(granted_session_id), None),
    );

    let RouterResult::Granted {
        has_replaced_active_grant,
        ..
    } = session_grant_result
    else {
        panic!("the grant on the session scope was refused: {session_grant_result:?}")
    };
    assert!(
        !has_replaced_active_grant,
        "ada held no grant on that session"
    );
    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    assert_eq!(written_token_store.token_records.len(), 2);
    assert_eq!(
        written_token_store.token_records[0].scope,
        TokenScope::HostWide
    );
    assert_eq!(
        written_token_store.token_records[1].scope,
        TokenScope::Session(granted_session_id)
    );
}

#[test]
fn a_grant_expires_the_given_span_after_the_clock_reading_it_was_issued_at() {
    // The router stamps `issued_at` and `expires_at` from one clock reading:
    // `expires_at` is exactly the requested span after `issued_at`.
    const ONE_DAY_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(runtime_directory.path());

    let grant_result = answer_token_request(
        Some(&token_store_path),
        build_grant_request("ada", TokenScope::HostWide, Some(ONE_DAY_DURATION)),
    );

    let RouterResult::Granted {
        has_replaced_active_grant,
        ..
    } = grant_result
    else {
        panic!("the grant was refused: {grant_result:?}")
    };
    assert!(
        !has_replaced_active_grant,
        "the store held no grant for ada to replace"
    );
    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    assert_eq!(written_token_store.token_records.len(), 1);
    let expires_at = written_token_store.token_records[0]
        .expires_at
        .expect("the grant carries an expiry");
    assert_eq!(
        expires_at
            .duration_since(written_token_store.token_records[0].issued_at)
            .expect("the expiry is after the issue time"),
        ONE_DAY_DURATION
    );

    let no_expiry_grant_result = answer_token_request(
        Some(&token_store_path),
        build_grant_request("grace", TokenScope::HostWide, None),
    );

    let RouterResult::Granted {
        has_replaced_active_grant,
        ..
    } = no_expiry_grant_result
    else {
        panic!("the grant was refused: {no_expiry_grant_result:?}")
    };
    assert!(
        !has_replaced_active_grant,
        "the store held no grant for grace to replace"
    );
    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    assert_eq!(written_token_store.token_records.len(), 2);
    assert_eq!(
        written_token_store.token_records[1].expires_at, None,
        "a grant with no span has no expiry"
    );
}

#[test]
fn a_span_the_clock_cannot_represent_is_refused_and_leaves_the_store_alone() {
    // An expiry past the clock's range is refused before any write: the store
    // file stays absent, or keeps its bytes.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(runtime_directory.path());
    let unrepresentable_grant_request = build_grant_request(
        "ada",
        TokenScope::HostWide,
        Some(Duration::from_secs(u64::MAX)),
    );
    let refusal_result = build_token_refusal_result(
        "the expiry is further ahead than this machine's clock can represent",
    );

    assert_eq!(
        answer_token_request(Some(&token_store_path), unrepresentable_grant_request),
        refusal_result
    );
    assert!(
        !token_store_path.exists(),
        "the refusal came before the store was created"
    );

    let _ = grant_token_for_test(&token_store_path, "ada", TokenScope::HostWide);
    let token_store_bytes_before_refused_request =
        rewrite_token_store_with_spacing(&token_store_path);
    let unrepresentable_grant_request = build_grant_request(
        "ada",
        TokenScope::HostWide,
        Some(Duration::from_secs(u64::MAX)),
    );

    assert_eq!(
        answer_token_request(Some(&token_store_path), unrepresentable_grant_request),
        refusal_result
    );
    assert_eq!(
        std::fs::read(&token_store_path).expect("the store file is still there"),
        token_store_bytes_before_refused_request,
        "the refused grant wrote nothing"
    );
}

#[test]
fn a_bare_revoke_stops_every_grant_the_identity_holds_in_the_stores_order() {
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(runtime_directory.path());
    let granted_session_id = SessionId::new();
    let _ = grant_token_for_test(&token_store_path, "ada", TokenScope::HostWide);
    let _ = grant_token_for_test(
        &token_store_path,
        "ada",
        TokenScope::Session(granted_session_id),
    );

    let revoke_started_at = SystemTime::now();
    let revoke_result = answer_token_request(
        Some(&token_store_path),
        RouterRequestKind::RevokeToken {
            identity: "ada".to_string(),
            scope: None,
        },
    );
    let revoke_finished_at = SystemTime::now();

    assert_eq!(
        revoke_result,
        RouterResult::Revoked(vec![
            TokenScope::HostWide,
            TokenScope::Session(granted_session_id)
        ])
    );
    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    assert_eq!(written_token_store.token_records.len(), 2);
    for token_record in &written_token_store.token_records {
        let revoked_at = token_record
            .revoked_at
            .expect("the revoke stamped this token record");
        assert!(
            revoked_at >= revoke_started_at && revoked_at <= revoke_finished_at,
            "the stamp is the clock reading the revoke took"
        );
    }
}

#[test]
fn a_scoped_revoke_stops_that_one_grant_and_leaves_the_other_standing() {
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(runtime_directory.path());
    let granted_session_id = SessionId::new();
    let _ = grant_token_for_test(&token_store_path, "ada", TokenScope::HostWide);
    let _ = grant_token_for_test(
        &token_store_path,
        "ada",
        TokenScope::Session(granted_session_id),
    );

    let revoke_started_at = SystemTime::now();
    let revoke_result = answer_token_request(
        Some(&token_store_path),
        RouterRequestKind::RevokeToken {
            identity: "ada".to_string(),
            scope: Some(TokenScope::Session(granted_session_id)),
        },
    );
    let revoke_finished_at = SystemTime::now();

    assert_eq!(
        revoke_result,
        RouterResult::Revoked(vec![TokenScope::Session(granted_session_id)])
    );
    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    assert_eq!(written_token_store.token_records.len(), 2);
    assert_eq!(
        written_token_store.token_records[0].scope,
        TokenScope::HostWide
    );
    assert_eq!(
        written_token_store.token_records[0].revoked_at, None,
        "the host-wide grant still stands"
    );
    assert_eq!(
        written_token_store.token_records[1].scope,
        TokenScope::Session(granted_session_id)
    );
    let revoked_at = written_token_store.token_records[1]
        .revoked_at
        .expect("the revoke stamped the session grant");
    assert!(
        revoked_at >= revoke_started_at && revoked_at <= revoke_finished_at,
        "the stamp is the clock reading the revoke took"
    );
}

#[test]
fn revoking_an_identity_that_holds_nothing_stops_nothing_and_writes_nothing() {
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(runtime_directory.path());
    let _ = grant_token_for_test(&token_store_path, "ada", TokenScope::HostWide);
    let token_store_bytes_before_empty_revoke = rewrite_token_store_with_spacing(&token_store_path);

    let revoke_result = answer_token_request(
        Some(&token_store_path),
        RouterRequestKind::RevokeToken {
            identity: "grace".to_string(),
            scope: None,
        },
    );

    assert_eq!(revoke_result, RouterResult::Revoked(Vec::new()));
    assert_eq!(
        std::fs::read(&token_store_path).expect("the store file is still there"),
        token_store_bytes_before_empty_revoke,
        "a revoke that stopped nothing wrote nothing"
    );
}

#[test]
fn listing_answers_every_grant_without_its_hash_and_narrows_to_one_scope() {
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(runtime_directory.path());
    let granted_session_id = SessionId::new();
    let other_session_id = SessionId::new();
    let _ = grant_token_for_test(&token_store_path, "ada", TokenScope::HostWide);
    let _ = grant_token_for_test(
        &token_store_path,
        "ada",
        TokenScope::Session(granted_session_id),
    );
    let _ = grant_token_for_test(
        &token_store_path,
        "grace",
        TokenScope::Session(other_session_id),
    );
    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    let build_token_entry = |identity: &str, scope: &TokenScope| {
        let token_record = written_token_store
            .token_records
            .iter()
            .find(|token_record| token_record.identity == identity && token_record.scope == *scope)
            .expect("the store holds this grant");
        TokenEntry {
            identity: token_record.identity.clone(),
            scope: token_record.scope.clone(),
            issued_at: token_record.issued_at,
            expires_at: token_record.expires_at,
            last_used_at: token_record.last_used_at,
            revoked_at: token_record.revoked_at,
        }
    };

    let full_listing_result = answer_token_request(
        Some(&token_store_path),
        RouterRequestKind::ListTokens { scope: None },
    );

    assert_eq!(
        full_listing_result,
        RouterResult::Tokens(vec![
            build_token_entry("ada", &TokenScope::HostWide),
            build_token_entry("ada", &TokenScope::Session(granted_session_id)),
            build_token_entry("grace", &TokenScope::Session(other_session_id)),
        ])
    );
    let encoded_json = serde_json::to_string(&full_listing_result).expect("the listing encodes");
    for token_record in &written_token_store.token_records {
        assert!(
            !encoded_json.contains(&token_record.token_hash),
            "a listed grant carries no hash"
        );
    }

    let narrowed_listing_result = answer_token_request(
        Some(&token_store_path),
        RouterRequestKind::ListTokens {
            scope: Some(TokenScope::Session(granted_session_id)),
        },
    );

    assert_eq!(
        narrowed_listing_result,
        RouterResult::Tokens(vec![
            build_token_entry("ada", &TokenScope::HostWide),
            build_token_entry("ada", &TokenScope::Session(granted_session_id)),
        ]),
        "a session scope lists every grant that reaches that session: ada's host-wide grant \
         and ada's grant on that session, and not grace's grant on another session"
    );
}

#[test]
fn a_store_holding_junk_refuses_every_token_request_and_changes_nothing() {
    // A store file that does not decode refuses the grant, the revoke and the
    // listing, and no request writes the file.
    const INVALID_TOKEN_STORE_BYTES: &[u8] = b"not a token store";
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(runtime_directory.path());
    std::fs::create_dir_all(
        token_store_path
            .parent()
            .expect("the store sits in a directory"),
    )
    .expect("the store's directory is made");
    std::fs::write(&token_store_path, INVALID_TOKEN_STORE_BYTES).expect("the junk is written");
    let token_store_error = TokenStore::load_token_store_from_path(&token_store_path)
        .expect_err("junk is not a readable store");
    let refusal_result = build_token_refusal_result(&token_store_error.to_string());

    for request_kind in list_token_request_kinds(SessionId::new()) {
        let request_kind_name = request_kind.get_request_kind_name();
        assert_eq!(
            answer_token_request(Some(&token_store_path), request_kind),
            refusal_result,
            "{request_kind_name} is refused"
        );
        assert_eq!(
            std::fs::read(&token_store_path).expect("the store file is still there"),
            INVALID_TOKEN_STORE_BYTES,
            "{request_kind_name} changed no byte of the store"
        );
    }
}

#[test]
fn a_machine_with_no_data_directory_refuses_every_token_request() {
    let refusal_result = build_token_refusal_result(
        "this machine has no data directory, so no remote access token can be stored",
    );

    for request_kind in list_token_request_kinds(SessionId::new()) {
        let request_kind_name = request_kind.get_request_kind_name();
        assert_eq!(
            answer_token_request(None, request_kind),
            refusal_result,
            "{request_kind_name} is refused"
        );
    }
}

/// What [`read_session_server_ready_line`] reads from a child that printed
/// `printed_line` on its standard output. The bytes cross a real pipe.
#[cfg(unix)]
fn read_ready_line_from_process(printed_line: &str) -> Option<SessionServerReady> {
    let runtime_directory = build_test_runtime_directory();
    let printed_file_path = runtime_directory.path().join("printed");
    std::fs::write(&printed_file_path, printed_line).expect("the output is written");
    let mut child_process = std::process::Command::new("/bin/cat")
        .arg(&printed_file_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("cat runs");
    let child_stdout = child_process
        .stdout
        .take()
        .expect("the child's output is piped");
    let ready_report = read_session_server_ready_line(child_stdout);
    let _ = child_process.wait();
    ready_report
}

#[cfg(unix)]
#[test]
fn the_line_a_session_server_prints_reads_back_as_its_report() {
    let ready_report = SessionServerReady {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        socket_address: "/tmp/koshi-test.sock".to_string(),
    };
    let ready_report_json = serde_json::to_string(&ready_report).expect("the report encodes");

    assert_eq!(
        read_ready_line_from_process(&format!("{ready_report_json}\n")),
        Some(ready_report)
    );
}

/// Every way the one line the router reads off a child's output fails to be a
/// report.
#[cfg(unix)]
#[test]
fn output_that_is_not_a_ready_report_reads_as_nothing() {
    assert_eq!(
        read_ready_line_from_process(""),
        None,
        "a child that printed nothing"
    );
    assert_eq!(
        read_ready_line_from_process("not json\n"),
        None,
        "a line that is not a report"
    );
    assert_eq!(
        read_ready_line_from_process("{\"protocol_version\":1}\n"),
        None,
        "a report naming no socket"
    );
    assert_eq!(
        read_ready_line_from_process(
            "{\"protocol_version\":\"one\",\"socket_address\":\"/tmp/s\"}\n"
        ),
        None,
        "a report whose version is not a number"
    );
}

#[test]
fn a_session_server_on_this_build_is_served() {
    let ready_report = SessionServerReady {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        socket_address: "/tmp/koshi-test.sock".to_string(),
    };

    let accepted_ready_report = validate_session_server_ready(Some(ready_report.clone()))
        .expect("this build's report is served");

    assert_eq!(accepted_ready_report, ready_report);
}

#[test]
fn a_session_server_that_printed_nothing_is_refused_as_no_bound_socket() {
    let refusal = validate_session_server_ready(None).expect_err("nothing readable is refused");

    assert_eq!(refusal, "the session did not report a bound socket");
}

#[test]
fn a_session_server_from_another_build_is_refused_naming_both_versions() {
    // A ready report on another control-plane version is refused with both
    // version numbers.
    let refusal = validate_session_server_ready(Some(SessionServerReady {
        protocol_version: ROUTER_PROTOCOL_VERSION + 1,
        socket_address: "/tmp/koshi-test.sock".to_string(),
    }))
    .expect_err("another build is refused");

    assert_eq!(
        refusal,
        format!(
            "the koshi binary on disk speaks control-plane protocol version {} and this running \
             router speaks {ROUTER_PROTOCOL_VERSION}, so they are different builds; the router \
             serves its own build until it restarts, which it does once no session is left \
             running",
            ROUTER_PROTOCOL_VERSION + 1
        )
    );
}

/// How long a test waits for a connection a revoke ends: 5 s.
const CONNECTION_END_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// How many loopback ports a test tries before it gives up opening the real
/// remote listener.
const MAX_ADDRESS_ATTEMPT_COUNT: usize = 8;

/// An address on the loopback interface nothing is listening on.
///
/// Binds port `0` on `127.0.0.1`, reads the port the OS picked, and releases
/// it. Another program can take the port before the caller binds it:
/// [`open_test_remote_listener`] and [`enable_remote_on_a_free_port`] try
/// again on a new port.
fn find_free_loopback_address() -> String {
    let probe_listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    probe_listener
        .local_addr()
        .expect("the address that was bound")
        .to_string()
}

/// Open the real remote listener on a loopback port, trying up to
/// [`MAX_ADDRESS_ATTEMPT_COUNT`] ports, and hand back the address it bound.
fn open_test_remote_listener(
    certificate_file: &CertFile,
    router_events_sender: &Sender<RouterEvent>,
) -> String {
    for _ in 0..MAX_ADDRESS_ATTEMPT_COUNT {
        let loopback_address = find_free_loopback_address();
        let bound_listener_result =
            remote_listener::bind_remote_listener(loopback_address.clone(), certificate_file);
        if let Ok(bound_listener) = bound_listener_result {
            bound_listener.start_serving(router_events_sender.clone());
            return loopback_address;
        }
    }
    panic!("no loopback port could be bound in {MAX_ADDRESS_ATTEMPT_COUNT} tries");
}

/// Switch remote access on at a free loopback port, trying up to
/// [`MAX_ADDRESS_ATTEMPT_COUNT`] ports.
///
/// Sets `remote_state.remote_listen_address` to each port it tries and leaves
/// it at the one that worked.
fn enable_remote_on_a_free_port(
    remote_state: &mut RemoteState,
    router_events_sender: &Sender<RouterEvent>,
) -> RouterResult {
    for _ in 0..MAX_ADDRESS_ATTEMPT_COUNT {
        remote_state.remote_listen_address = Some(find_free_loopback_address());
        let remote_enable_result = enable_remote_access(remote_state, router_events_sender);
        if matches!(remote_enable_result, RouterResult::RemoteEnabled { .. }) {
            return remote_enable_result;
        }
    }
    panic!("no loopback port could be enabled in {MAX_ADDRESS_ATTEMPT_COUNT} tries");
}

/// A stand-in session server behind the bridge, bound at `socket_address`. It
/// accepts the connection the router opens, answers the Hello the router
/// presents for the remote client, and holds the connection open until the
/// sender of `stop_receiver` drops.
fn spawn_bridged_session_server(
    socket_address: &str,
    stop_receiver: Receiver<()>,
) -> JoinHandle<()> {
    let listener = Listener::bind(socket_address).expect("bind the session behind the bridge");
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the router's bridge");
        let hello_request: IpcRequest = connection
            .recv()
            .expect("read the hello the router presents");
        connection
            .send(&IpcResponse {
                request_id: Some(hello_request.request_id),
                answer_result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: env!("CARGO_PKG_VERSION").to_string(),
                },
            })
            .expect("answer the hello");
        let _ = stop_receiver.recv();
    })
}

/// Write a token store at `token_store_path` holding one host-wide grant for
/// alice with no expiry, and hand back the secret it made.
fn build_token_store_with_alice_grant(token_store_path: &Path) -> ConnectionToken {
    let mut token_store = TokenStore::new();
    let (connection_token, _) = token_store.grant_token(
        "alice".to_string(),
        TokenScope::HostWide,
        SystemTime::now(),
        None,
    );
    token_store
        .write_token_store_to_path(token_store_path)
        .expect("the token store is written");
    connection_token
}

/// The two ends of one loopback TCP connection: the caller end, then the
/// served end.
fn build_loopback_connection_pair() -> (TcpStream, TcpStream) {
    let loopback_listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let loopback_address = loopback_listener
        .local_addr()
        .expect("the address that was bound");
    let caller_stream = TcpStream::connect(loopback_address).expect("the caller connects");
    let (served_stream, _) = loopback_listener
        .accept()
        .expect("the connection is accepted");
    (caller_stream, served_stream)
}

/// Raise the Unix soft file-descriptor limit to the hard limit, once per test
/// process.
#[cfg(unix)]
fn raise_router_test_file_descriptor_limit() {
    static FILE_DESCRIPTOR_LIMIT_INITIALIZER: std::sync::Once = std::sync::Once::new();

    FILE_DESCRIPTOR_LIMIT_INITIALIZER.call_once(|| {
        let mut resource_limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let getrlimit_result = unsafe {
            libc::getrlimit(
                libc::RLIMIT_NOFILE,
                &mut resource_limit as *mut libc::rlimit,
            )
        };
        assert_eq!(
            getrlimit_result, 0,
            "read the router test file-descriptor limit"
        );

        if resource_limit.rlim_cur < resource_limit.rlim_max {
            resource_limit.rlim_cur = resource_limit.rlim_max;
            let setrlimit_result = unsafe {
                libc::setrlimit(libc::RLIMIT_NOFILE, &resource_limit as *const libc::rlimit)
            };
            assert_eq!(
                setrlimit_result, 0,
                "raise the router test file-descriptor limit"
            );
        }
    });
}

#[cfg(not(unix))]
fn raise_router_test_file_descriptor_limit() {}

#[test]
fn a_revoke_ends_the_connection_it_admitted_attached_or_not() {
    // A revoke ends every connection its grant admitted: one that attached to a
    // session, and one that only listed the sessions.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let token_store_path = resolve_token_store_path(&data_directory);
    let session_id = SessionId::new();

    let connection_token = build_token_store_with_alice_grant(&token_store_path);
    let (certificate_file, certificate_fingerprint) =
        load_or_create_certificate(&data_directory).expect("this machine's certificate");

    let socket_address = compute_socket_address(runtime_directory.path(), session_id);
    let (stop_sender, stop_receiver) = mpsc::channel();
    let session_server_thread = spawn_bridged_session_server(&socket_address, stop_receiver);
    EndpointFile {
        socket_address,
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory.path(),
        session_id,
    ))
    .expect("the endpoint file is written");

    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let remote_listen_address = open_test_remote_listener(&certificate_file, &router_events_sender);
    let test_router_event_sender = router_events_sender.clone();
    let held_runtime_directory = runtime_directory.path().to_path_buf();
    let held_token_store_path = token_store_path.clone();
    let held_remote_listen_address = remote_listen_address.clone();
    let held_data_directory = data_directory.clone();
    let loop_thread = std::thread::spawn(move || {
        let mut session_registry = build_session_registry(&[(session_id, "S-quiet-lake")]);
        let mut remote_state = RemoteState {
            remote_listen_address: Some(held_remote_listen_address),
            data_directory: Some(held_data_directory),
            is_listening: true,
            admitted_remote_connections: Vec::new(),
            next_remote_connection_id: 0,
            full_capacity_warning: WarningRateLimiter::new(),
        };
        run_dispatch_loop(
            &held_runtime_directory,
            &get_test_executable_path(),
            Some(&held_token_store_path),
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            &mut session_registry,
            &mut remote_state,
        )
    });

    // The first connection attaches to the session through the router's
    // bridge.
    let attaching_connection = remote_client::connect_remote_server(
        &remote_listen_address,
        &connection_token,
        Some(&certificate_fingerprint),
        DIAL_TIMEOUT_DURATION,
        None,
    )
    .expect("the secret is admitted");
    let (mut bridged_connection, _bridged_writer) = remote_client::attach_remote_session(
        attaching_connection,
        SessionSelector::SessionId(session_id),
    )
    .expect("the attach is sent");
    let hello_response: IncomingResponse = bridged_connection
        .recv()
        .expect("the session answers the hello");
    assert_eq!(hello_response.request_id, Some(1), "the bridge stands");

    // The second connection lists the sessions and keeps the connection open.
    let mut listing_connection = remote_client::connect_remote_server(
        &remote_listen_address,
        &connection_token,
        Some(&certificate_fingerprint),
        DIAL_TIMEOUT_DURATION,
        None,
    )
    .expect("the secret is admitted");
    let remote_session_rows = remote_client::list_remote_sessions(&mut listing_connection)
        .expect("the sessions are listed");
    assert_eq!(
        remote_session_rows
            .iter()
            .map(|remote_session_row| remote_session_row.session_id)
            .collect::<Vec<_>>(),
        vec![session_id],
        "the host-wide grant reaches this machine's one session"
    );

    // Both connections block on their next read.
    let (bridged_end_sender, bridged_end_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = bridged_end_sender.send(bridged_connection.recv::<IncomingResponse>().is_err());
    });
    let mut listing_reader = listing_connection.frame_reader;
    let (listing_end_sender, listing_end_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = listing_end_sender.send(listing_reader.recv::<RemoteServerFrame>().is_err());
    });

    let (response_sender, revoke_result_receiver) = mpsc::channel();
    test_router_event_sender
        .send(RouterEvent::Request {
            request_kind: RouterRequestKind::RevokeToken {
                identity: "alice".to_string(),
                scope: None,
            },
            response_sender,
        })
        .expect("the revoke is queued");
    assert_eq!(
        revoke_result_receiver
            .recv()
            .expect("the revoke is answered"),
        RouterResult::Revoked(vec![TokenScope::HostWide])
    );

    assert!(
        listing_end_receiver
            .recv_timeout(CONNECTION_END_TIMEOUT_DURATION)
            .unwrap_or_else(|_| panic!(
                "the connection that only listed is still reading {CONNECTION_END_TIMEOUT_DURATION:?} after the revoke"
            )),
        "the connection that only listed ended at the revoke"
    );
    assert!(
        bridged_end_receiver
            .recv_timeout(CONNECTION_END_TIMEOUT_DURATION)
            .unwrap_or_else(|_| panic!(
                "the connection carrying a session is still reading {CONNECTION_END_TIMEOUT_DURATION:?} after the revoke"
            )),
        "the connection carrying a session ended at the revoke"
    );

    test_router_event_sender
        .send(RouterEvent::ChildExited(session_id))
        .expect("the exit is queued");
    assert_eq!(
        loop_thread.join().expect("the loop ended"),
        RouterExit::Idle
    );
    drop(stop_sender);
    session_server_thread
        .join()
        .expect("the stand-in session ended");
}

#[test]
fn a_grant_cuts_only_the_standing_connection_of_its_identity_and_scope() {
    // The store holds five records. The grant for alice on `HostWide` replaces
    // the one active record on that identity and scope, and ends its
    // connection. A revoked record, an expired record, another scope and
    // another identity keep their connections.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let token_store_path = resolve_token_store_path(&data_directory);
    std::fs::create_dir_all(&data_directory).expect("create the data directory");

    let current_time = SystemTime::now();
    let one_hour_duration = Duration::from_secs(3600);
    let build_token_record =
        |identity: &str,
         token_hash_character: char,
         scope: TokenScope,
         expires_at: Option<SystemTime>,
         revoked_at: Option<SystemTime>| TokenRecord {
            identity: identity.to_string(),
            token_hash: token_hash_character.to_string().repeat(64),
            scope,
            issued_at: current_time - one_hour_duration,
            expires_at,
            last_used_at: None,
            revoked_at,
        };
    let other_session_id = SessionId::new();
    let mut token_store = TokenStore::new();
    token_store.token_records.push(build_token_record(
        "alice",
        'a',
        TokenScope::HostWide,
        None,
        None,
    ));
    token_store.token_records.push(build_token_record(
        "alice",
        'b',
        TokenScope::HostWide,
        None,
        Some(current_time),
    ));
    token_store.token_records.push(build_token_record(
        "alice",
        'c',
        TokenScope::HostWide,
        Some(current_time - one_hour_duration),
        None,
    ));
    token_store.token_records.push(build_token_record(
        "alice",
        'd',
        TokenScope::Session(other_session_id),
        None,
        None,
    ));
    token_store.token_records.push(build_token_record(
        "bob",
        'e',
        TokenScope::HostWide,
        None,
        None,
    ));
    token_store
        .write_token_store_to_path(&token_store_path)
        .expect("the store is written");

    let mut remote_state = build_no_remote_state();
    let mut held_connection_streams = Vec::new();
    for (remote_connection_index, token_hash_character) in
        ['a', 'b', 'c', 'd', 'e'].into_iter().enumerate()
    {
        let (caller_stream, served_stream) = build_loopback_connection_pair();
        held_connection_streams.push(caller_stream);
        remote_state
            .admitted_remote_connections
            .push(AdmittedRemoteConnection {
                token_hash: token_hash_character.to_string().repeat(64),
                tcp_stream: served_stream,
                remote_connection_id: remote_connection_index as u64,
            });
    }

    let grant_result = grant_token(
        Some(&token_store_path),
        &mut remote_state,
        "alice".to_string(),
        TokenScope::HostWide,
        None,
    );

    let RouterResult::Granted {
        connection_token,
        has_replaced_active_grant,
    } = grant_result
    else {
        panic!("the grant was refused: {grant_result:?}")
    };
    assert!(
        has_replaced_active_grant,
        "the standing alice grant is reported replaced"
    );
    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    assert_eq!(
        written_token_store
            .token_records
            .iter()
            .filter(|token_record| token_record.identity == "alice"
                && token_record.scope == TokenScope::HostWide)
            .map(|token_record| token_record.token_hash.clone())
            .collect::<Vec<String>>(),
        vec![hash_connection_token(&connection_token)],
        "one alice token record is left on the host-wide scope, holding the new secret"
    );
    let kept_token_hashes: Vec<String> = remote_state
        .admitted_remote_connections
        .iter()
        .map(|admitted_connection| admitted_connection.token_hash.clone())
        .collect();
    assert_eq!(
        kept_token_hashes,
        vec![
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            "e".repeat(64),
        ],
        "only the replaced token's connection was cut"
    );
}

#[test]
fn an_attach_that_arrives_after_the_cut_is_refused_rather_than_bridged() {
    // The locate for an attach checks the connection's registration before it
    // resolves the session. Closing the connections for the token hash removes
    // that registration: the same locate then answers `None`.
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let session_registry = build_session_registry(&[(session_id, "S-quiet-lake")]);
    let token_hash = "b".repeat(64);
    let mut remote_state = build_no_remote_state();
    let (_caller_stream, served_stream) = build_loopback_connection_pair();
    remote_state
        .admitted_remote_connections
        .push(AdmittedRemoteConnection {
            token_hash: token_hash.clone(),
            tcp_stream: served_stream,
            remote_connection_id: 7,
        });

    let remote_session_before_close = locate_remote_session(
        runtime_directory.path(),
        &session_registry,
        &remote_state,
        &TokenScope::HostWide,
        7,
        &SessionSelector::SessionId(session_id),
    );
    remote_state.close_connections_for_token_hashes(&[token_hash]);
    let remote_session_after_close = locate_remote_session(
        runtime_directory.path(),
        &session_registry,
        &remote_state,
        &TokenScope::HostWide,
        7,
        &SessionSelector::SessionId(session_id),
    );

    assert_eq!(
        remote_session_before_close,
        Some(EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            session_id
        )),
        "the same attach reached the session while the connection stood"
    );
    assert_eq!(
        remote_session_after_close, None,
        "the cut connection reaches nothing"
    );
    assert_eq!(
        remote_state.admitted_remote_connections.len(),
        0,
        "the closed connection left the list"
    );
}

#[test]
fn a_secret_on_one_session_is_shown_that_session_and_no_other() {
    // A grant on one session lists that session and no other.
    let reached_session_id = SessionId::new();
    let other_session_id = SessionId::new();
    let session_registry = build_session_registry(&[
        (reached_session_id, "S-quiet-lake"),
        (other_session_id, "S-loud-river"),
    ]);

    assert_eq!(
        list_remote_session_rows(&session_registry, &TokenScope::Session(reached_session_id)),
        vec![RemoteSessionRow {
            session_id: reached_session_id,
            session_name: "S-quiet-lake".to_string(),
        }]
    );
    assert_eq!(
        list_remote_session_rows(&session_registry, &TokenScope::Session(SessionId::new())),
        Vec::<RemoteSessionRow>::new(),
        "a grant on a session this machine does not run is shown nothing"
    );
    assert_eq!(
        list_remote_session_rows(&session_registry, &TokenScope::HostWide),
        vec![
            RemoteSessionRow {
                session_id: other_session_id,
                session_name: "S-loud-river".to_string(),
            },
            RemoteSessionRow {
                session_id: reached_session_id,
                session_name: "S-quiet-lake".to_string(),
            },
        ],
        "a host-wide grant is shown every session, in name order"
    );
    assert_eq!(
        list_remote_session_rows(&SessionRegistry::new(), &TokenScope::HostWide),
        Vec::<RemoteSessionRow>::new(),
        "and a machine running nothing is shown nothing"
    );
}

#[test]
fn two_sessions_carrying_one_name_are_listed_in_id_order() {
    // Two sessions with one name are listed in session id order.
    let mut ordered_session_ids = [SessionId::new(), SessionId::new()];
    ordered_session_ids.sort();
    let [first_session_id, second_session_id] = ordered_session_ids;
    let session_registry = build_session_registry(&[
        (second_session_id, "S-quiet-lake"),
        (first_session_id, "S-quiet-lake"),
    ]);

    assert_eq!(
        list_remote_session_rows(&session_registry, &TokenScope::HostWide),
        vec![
            RemoteSessionRow {
                session_id: first_session_id,
                session_name: "S-quiet-lake".to_string(),
            },
            RemoteSessionRow {
                session_id: second_session_id,
                session_name: "S-quiet-lake".to_string(),
            },
        ]
    );
}

#[test]
fn an_attach_to_a_session_the_secret_does_not_reach_is_refused() {
    // The connection stands and the session is running: the scope alone
    // refuses the attach.
    let reached_session_id = SessionId::new();
    let other_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_registry = build_session_registry(&[
        (reached_session_id, "S-quiet-lake"),
        (other_session_id, "S-loud-river"),
    ]);
    let mut remote_state = build_no_remote_state();
    let (_caller_stream, served_stream) = build_loopback_connection_pair();
    remote_state
        .admitted_remote_connections
        .push(AdmittedRemoteConnection {
            token_hash: "c".repeat(64),
            tcp_stream: served_stream,
            remote_connection_id: 3,
        });
    let locate_session = |scope: &TokenScope, session_selector: &SessionSelector| {
        locate_remote_session(
            runtime_directory.path(),
            &session_registry,
            &remote_state,
            scope,
            3,
            session_selector,
        )
    };

    assert_eq!(
        locate_session(
            &TokenScope::Session(reached_session_id),
            &SessionSelector::SessionId(reached_session_id)
        ),
        Some(EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            reached_session_id
        )),
        "the session the grant names is reached"
    );
    assert_eq!(
        locate_session(
            &TokenScope::Session(reached_session_id),
            &SessionSelector::SessionId(other_session_id)
        ),
        None,
        "the session beside it is outside the grant"
    );
    assert_eq!(
        locate_session(
            &TokenScope::Session(reached_session_id),
            &SessionSelector::SessionName("S-loud-river".to_string())
        ),
        None,
        "and naming that session instead reaches nothing either"
    );
    assert_eq!(
        locate_session(
            &TokenScope::HostWide,
            &SessionSelector::SessionId(other_session_id)
        ),
        Some(EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            other_session_id
        )),
        "a host-wide grant reaches both"
    );
}

#[test]
fn an_attach_naming_a_session_this_machine_does_not_run_reaches_nothing() {
    // The connection stands and a host-wide grant reaches every session: the
    // session selector alone refuses the attach.
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_registry = build_session_registry(&[(running_session_id, "S-quiet-lake")]);
    let mut remote_state = build_no_remote_state();
    let (_caller_stream, served_stream) = build_loopback_connection_pair();
    remote_state
        .admitted_remote_connections
        .push(AdmittedRemoteConnection {
            token_hash: "f".repeat(64),
            tcp_stream: served_stream,
            remote_connection_id: 9,
        });
    let locate_session = |session_selector: &SessionSelector| {
        locate_remote_session(
            runtime_directory.path(),
            &session_registry,
            &remote_state,
            &TokenScope::HostWide,
            9,
            session_selector,
        )
    };

    assert_eq!(
        locate_session(&SessionSelector::SessionName("S-loud-river".to_string())),
        None,
        "a name the list does not hold"
    );
    assert_eq!(
        locate_session(&SessionSelector::SessionName("S-quiet".to_string())),
        None,
        "a name that is only the start of one the list holds"
    );
    assert_eq!(
        locate_session(&SessionSelector::SessionId(SessionId::new())),
        None,
        "an id the list does not hold"
    );
    assert_eq!(
        locate_session(&SessionSelector::SessionId(running_session_id)),
        Some(EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            running_session_id
        )),
        "and the session that is running is still reached"
    );
}

#[test]
fn a_session_another_local_user_started_is_neither_listed_nor_reached_from_a_remote_connection() {
    // The rebuild registers another local user's session with process id `0`.
    // A remote connection neither lists nor reaches a session with process id
    // `0`.
    let own_session_id = SessionId::new();
    let foreign_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut session_registry = build_session_registry(&[(own_session_id, "S-quiet-lake")]);
    session_registry.insert(
        foreign_session_id,
        SessionRecord {
            session_name: "S-loud-river".to_string(),
            socket_address: compute_socket_address(Path::new("/nowhere"), foreign_session_id),
            process_id: 0,
        },
    );
    let mut remote_state = build_no_remote_state();
    let (_caller_stream, served_stream) = build_loopback_connection_pair();
    remote_state
        .admitted_remote_connections
        .push(AdmittedRemoteConnection {
            token_hash: "a".repeat(64),
            tcp_stream: served_stream,
            remote_connection_id: 5,
        });
    let locate_session = |session_selector: &SessionSelector| {
        locate_remote_session(
            runtime_directory.path(),
            &session_registry,
            &remote_state,
            &TokenScope::HostWide,
            5,
            session_selector,
        )
    };

    assert_eq!(
        list_remote_session_rows(&session_registry, &TokenScope::HostWide),
        vec![RemoteSessionRow {
            session_id: own_session_id,
            session_name: "S-quiet-lake".to_string(),
        }],
        "a host-wide grant is shown the session this router started and no other"
    );
    assert_eq!(
        list_remote_session_rows(&session_registry, &TokenScope::Session(foreign_session_id)),
        Vec::<RemoteSessionRow>::new(),
        "and a grant naming that session outright is shown nothing"
    );
    assert_eq!(
        locate_session(&SessionSelector::SessionId(foreign_session_id)),
        None,
        "an attach naming it by id reaches nothing"
    );
    assert_eq!(
        locate_session(&SessionSelector::SessionName("S-loud-river".to_string())),
        None,
        "and naming it reaches nothing either"
    );
    assert_eq!(
        locate_session(&SessionSelector::SessionId(own_session_id)),
        Some(EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            own_session_id
        )),
        "while the session this router started is still reached"
    );
}

#[test]
fn the_report_that_one_connection_ended_drops_that_registration_and_no_other() {
    // The listener sends `RemoveConnection` when a remote connection closes.
    // The router removes that one registration and keeps the others with their
    // ids.
    let runtime_directory = build_test_runtime_directory();
    let mut remote_state = build_no_remote_state();
    let mut held_connection_streams = Vec::new();
    for remote_connection_id in 0..3u64 {
        let (caller_stream, served_stream) = build_loopback_connection_pair();
        held_connection_streams.push(caller_stream);
        remote_state
            .admitted_remote_connections
            .push(AdmittedRemoteConnection {
                token_hash: "g".repeat(64),
                tcp_stream: served_stream,
                remote_connection_id,
            });
    }

    serve_remote_admission(
        runtime_directory.path(),
        None,
        &mut SessionRegistry::new(),
        &mut remote_state,
        AdmissionAsk::RemoveConnection {
            remote_connection_id: 1,
        },
    );

    assert_eq!(
        remote_state
            .admitted_remote_connections
            .iter()
            .map(|admitted_connection| admitted_connection.remote_connection_id)
            .collect::<Vec<u64>>(),
        vec![0, 2]
    );
}

#[test]
fn a_caller_speaking_no_remote_protocol_version_this_build_has_is_told_both_ranges() {
    // The listener checks the version before the secret: no grant and no
    // dispatcher take part. The refusal names both version ranges and differs
    // from `REMOTE_REFUSED`.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let (certificate_file, certificate_fingerprint) =
        load_or_create_certificate(&data_directory).expect("this machine's certificate");

    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let remote_listen_address = open_test_remote_listener(&certificate_file, &router_events_sender);

    let offered_remote_protocol_version = REMOTE_PROTOCOL_VERSION + 1;
    let hello_frame = RemoteClientFrame::Hello {
        minimum_remote_version: offered_remote_protocol_version,
        maximum_remote_version: offered_remote_protocol_version + 1,
        minimum_protocol_version: MIN_PROTOCOL_VERSION,
        maximum_protocol_version: PROTOCOL_VERSION,
        connection_token: ConnectionToken::generate(),
    };
    let (_frame_reader, _frame_writer, _presented_certificate_fingerprint, remote_server_response) =
        remote_wire::open_remote_connection(
            &remote_listen_address,
            Some(&certificate_fingerprint),
            &hello_frame,
            DIAL_TIMEOUT_DURATION,
            None,
        )
        .expect("the server answers the opening frame");

    let RemoteServerFrame::Refused { message } = remote_server_response else {
        panic!(
            "a remote protocol range with no overlap is refused, and got {remote_server_response:?}"
        );
    };
    assert_eq!(
        message,
        format!(
            "the caller speaks remote protocol versions {offered_remote_protocol_version} to {}, \
             this koshi speaks {MIN_REMOTE_PROTOCOL_VERSION} to {REMOTE_PROTOCOL_VERSION}",
            offered_remote_protocol_version + 1
        )
    );
    assert_ne!(
        message, REMOTE_REFUSED,
        "a version refusal is not the sentence a wrong secret gets"
    );
}

#[test]
fn a_caller_whose_remote_protocol_range_covers_this_build_settles_on_what_both_speak() {
    // The Welcome names the highest version both ends speak.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let token_store_path = resolve_token_store_path(&data_directory);

    let connection_token = build_token_store_with_alice_grant(&token_store_path);
    let (certificate_file, certificate_fingerprint) =
        load_or_create_certificate(&data_directory).expect("this machine's certificate");

    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let remote_listen_address = open_test_remote_listener(&certificate_file, &router_events_sender);
    let held_runtime_directory = runtime_directory.path().to_path_buf();
    let held_token_store_path = token_store_path.clone();
    let held_remote_listen_address = remote_listen_address.clone();
    let held_data_directory = data_directory.clone();
    let loop_thread = std::thread::spawn(move || {
        let mut session_registry = build_session_registry(&[]);
        let mut remote_state = RemoteState {
            remote_listen_address: Some(held_remote_listen_address),
            data_directory: Some(held_data_directory),
            is_listening: true,
            admitted_remote_connections: Vec::new(),
            next_remote_connection_id: 0,
            full_capacity_warning: WarningRateLimiter::new(),
        };
        run_dispatch_loop(
            &held_runtime_directory,
            &get_test_executable_path(),
            Some(&held_token_store_path),
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            &mut session_registry,
            &mut remote_state,
        )
    });

    // A caller that speaks this build's version and one above it.
    let hello_frame = RemoteClientFrame::Hello {
        minimum_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        maximum_remote_version: REMOTE_PROTOCOL_VERSION + 1,
        minimum_protocol_version: MIN_PROTOCOL_VERSION,
        maximum_protocol_version: PROTOCOL_VERSION,
        connection_token: connection_token.clone(),
    };
    let (_frame_reader, _frame_writer, _presented_certificate_fingerprint, remote_server_response) =
        remote_wire::open_remote_connection(
            &remote_listen_address,
            Some(&certificate_fingerprint),
            &hello_frame,
            DIAL_TIMEOUT_DURATION,
            None,
        )
        .expect("the server answers the opening frame");

    assert_eq!(
        remote_server_response,
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        },
        "the settled version is the highest both ends speak, not the caller's highest"
    );

    drop(loop_thread);
}

#[test]
fn an_admitted_secret_is_registered_with_its_scope_and_stamped_in_the_store() {
    // The admit registers the connection with its scope and the hash of its
    // secret, and stamps `last_used_at` on the token record.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(&runtime_directory.path().join("data"));
    let connection_token = build_token_store_with_alice_grant(&token_store_path);
    let mut remote_state = build_no_remote_state();
    let (caller_stream, _served_stream) = build_loopback_connection_pair();

    let admit_started_at = SystemTime::now();
    let connection_admission = admit_remote_token(
        Some(&token_store_path),
        &mut remote_state,
        &connection_token,
        caller_stream,
    );
    let admit_finished_at = SystemTime::now();

    assert_eq!(
        connection_admission,
        Some(RemoteConnectionAdmission {
            scope: TokenScope::HostWide,
            remote_connection_id: 0,
        })
    );
    assert_eq!(
        remote_state.next_remote_connection_id, 1,
        "the next connection takes the number 1"
    );
    assert_eq!(remote_state.admitted_remote_connections.len(), 1);
    assert_eq!(
        remote_state.admitted_remote_connections[0].remote_connection_id,
        0
    );
    assert_eq!(
        remote_state.admitted_remote_connections[0].token_hash,
        hash_connection_token(&connection_token),
        "the connection is registered against the hash of the secret that opened it"
    );
    let written_token_store =
        TokenStore::load_token_store_from_path(&token_store_path).expect("the store reads back");
    assert_eq!(written_token_store.token_records.len(), 1);
    let last_used_at = written_token_store.token_records[0]
        .last_used_at
        .expect("the admit stamped the token record");
    assert!(
        last_used_at >= admit_started_at && last_used_at <= admit_finished_at,
        "the stamp is the clock reading the admit took"
    );
}

#[test]
fn a_secret_the_store_does_not_hold_admits_nothing_and_writes_nothing() {
    // An unknown secret registers no connection and writes nothing to the
    // store.
    let runtime_directory = build_test_runtime_directory();
    let token_store_path = resolve_token_store_path(&runtime_directory.path().join("data"));
    let _ = build_token_store_with_alice_grant(&token_store_path);
    let token_store_bytes_before_unknown_secret =
        rewrite_token_store_with_spacing(&token_store_path);
    let mut remote_state = build_no_remote_state();
    let (caller_stream, _served_stream) = build_loopback_connection_pair();

    assert_eq!(
        admit_remote_token(
            Some(&token_store_path),
            &mut remote_state,
            &ConnectionToken::generate(),
            caller_stream
        ),
        None,
        "a secret no token record holds reaches nothing"
    );

    assert_eq!(
        remote_state.admitted_remote_connections.len(),
        0,
        "nothing was registered for it"
    );
    assert_eq!(
        remote_state.next_remote_connection_id, 0,
        "and it took no number"
    );
    assert_eq!(
        std::fs::read(&token_store_path).expect("the store file is still there"),
        token_store_bytes_before_unknown_secret,
        "the refused secret wrote nothing"
    );
}

#[test]
fn a_machine_with_no_token_store_admits_no_remote_connection() {
    // A machine with no data directory has no token store: every remote
    // caller is refused.
    let mut remote_state = build_no_remote_state();
    let (caller_stream, _served_stream) = build_loopback_connection_pair();

    assert_eq!(
        admit_remote_token(
            None,
            &mut remote_state,
            &ConnectionToken::generate(),
            caller_stream,
        ),
        None
    );

    assert_eq!(
        remote_state.admitted_remote_connections.len(),
        0,
        "nothing was registered for it"
    );
    assert_eq!(
        remote_state.next_remote_connection_id, 0,
        "and it took no number"
    );
}

#[test]
fn a_full_list_of_admitted_connections_admits_nothing_more() {
    // One valid secret, admitted MAX_LIVE_REMOTE_CONNECTION_COUNT times, then refused.
    raise_router_test_file_descriptor_limit();
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let token_store_path = resolve_token_store_path(&data_directory);

    let connection_token = build_token_store_with_alice_grant(&token_store_path);

    let mut remote_state = build_no_remote_state();
    for admission_index in 0..MAX_LIVE_REMOTE_CONNECTION_COUNT {
        let (caller_stream, served_stream) = build_loopback_connection_pair();
        let connection_admission = admit_remote_token(
            Some(&token_store_path),
            &mut remote_state,
            &connection_token,
            caller_stream,
        );
        assert_eq!(
            connection_admission,
            Some(RemoteConnectionAdmission {
                scope: TokenScope::HostWide,
                remote_connection_id: admission_index as u64,
            }),
            "admission {admission_index} of {MAX_LIVE_REMOTE_CONNECTION_COUNT} takes the next number"
        );
        drop(served_stream);
    }
    assert_eq!(
        remote_state.admitted_remote_connections.len(),
        MAX_LIVE_REMOTE_CONNECTION_COUNT
    );

    let (caller_stream, served_stream) = build_loopback_connection_pair();
    assert_eq!(
        admit_remote_token(
            Some(&token_store_path),
            &mut remote_state,
            &connection_token,
            caller_stream,
        ),
        None,
        "a good secret arriving at a full list is refused"
    );
    drop(served_stream);
    assert_eq!(
        remote_state.admitted_remote_connections.len(),
        MAX_LIVE_REMOTE_CONNECTION_COUNT,
        "and nothing was registered for it"
    );
    assert_eq!(
        remote_state.next_remote_connection_id, MAX_LIVE_REMOTE_CONNECTION_COUNT as u64,
        "and the refused connection took no number"
    );
}

#[test]
fn a_connection_that_ends_makes_room_for_the_next_one() {
    // A `RemoveConnection` question drops a registration and frees its place.
    raise_router_test_file_descriptor_limit();
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let token_store_path = resolve_token_store_path(&data_directory);

    let connection_token = build_token_store_with_alice_grant(&token_store_path);

    let mut remote_state = build_no_remote_state();
    let mut first_remote_connection_id = None;
    for _ in 0..MAX_LIVE_REMOTE_CONNECTION_COUNT {
        let (caller_stream, served_stream) = build_loopback_connection_pair();
        let connection_admission = admit_remote_token(
            Some(&token_store_path),
            &mut remote_state,
            &connection_token,
            caller_stream,
        )
        .expect("the list starts empty");
        first_remote_connection_id.get_or_insert(connection_admission.remote_connection_id);
        drop(served_stream);
    }
    let (caller_stream, served_stream) = build_loopback_connection_pair();
    assert_eq!(
        admit_remote_token(
            Some(&token_store_path),
            &mut remote_state,
            &connection_token,
            caller_stream,
        ),
        None
    );
    drop(served_stream);

    let ended_remote_connection_id =
        first_remote_connection_id.expect("a full list has a first connection");
    serve_remote_admission(
        runtime_directory.path(),
        Some(&token_store_path),
        &mut SessionRegistry::new(),
        &mut remote_state,
        AdmissionAsk::RemoveConnection {
            remote_connection_id: ended_remote_connection_id,
        },
    );
    assert_eq!(
        remote_state.admitted_remote_connections.len(),
        MAX_LIVE_REMOTE_CONNECTION_COUNT - 1
    );

    let (caller_stream, served_stream) = build_loopback_connection_pair();
    let connection_admission = admit_remote_token(
        Some(&token_store_path),
        &mut remote_state,
        &connection_token,
        caller_stream,
    );
    drop(served_stream);
    assert_eq!(
        connection_admission,
        Some(RemoteConnectionAdmission {
            scope: TokenScope::HostWide,
            remote_connection_id: MAX_LIVE_REMOTE_CONNECTION_COUNT as u64,
        }),
        "the connection taking that place takes the next number, not the freed one"
    );
}

#[test]
fn switching_remote_access_on_with_no_listen_address_is_refused() {
    // With no `remote-listen` address in `koshi.kdl`, the refusal names the
    // line to add.
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = build_no_remote_state();

    let enable_result = enable_remote_access(&mut remote_state, &router_events_sender);

    assert_eq!(
        enable_result,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "no remote listen address is set; add `remote-listen \"<host:port>\"` to \
                      koshi.kdl"
                .to_string(),
        })
    );
    assert!(!remote_state.is_listening, "no port is open");
}

#[test]
fn switching_remote_access_on_with_no_data_directory_is_refused() {
    // The certificate and the enabled file both live in the data directory. A
    // machine with no data directory is refused.
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = build_no_remote_state();
    remote_state.remote_listen_address = Some("127.0.0.1:7654".to_string());

    let enable_result = enable_remote_access(&mut remote_state, &router_events_sender);

    assert_eq!(
        enable_result,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "this machine has no data directory, so remote access cannot be switched on"
                .to_string(),
        })
    );
    assert!(!remote_state.is_listening, "no port is open");
}

#[test]
fn switching_remote_access_on_while_this_router_already_holds_the_port_keeps_serving_on_it() {
    // `is_listening` is already `true`: the router skips the bind, and an
    // address another socket holds is not refused.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let (_certificate_file, certificate_fingerprint) =
        load_or_create_certificate(&data_directory).expect("this machine's certificate");
    let occupied_listener = TcpListener::bind("127.0.0.1:0").expect("hold a loopback address");
    let remote_listen_address = occupied_listener
        .local_addr()
        .expect("read the held address")
        .to_string();
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = RemoteState {
        remote_listen_address: Some(remote_listen_address.clone()),
        data_directory: Some(data_directory.clone()),
        is_listening: true,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };

    let enable_result = enable_remote_access(&mut remote_state, &router_events_sender);

    assert_eq!(
        enable_result,
        RouterResult::RemoteEnabled {
            remote_listen_address,
            certificate_fingerprint,
        }
    );
    assert!(
        remote_state.is_listening,
        "the port it already held stays open"
    );
    assert!(
        is_remote_enabled(&data_directory),
        "the enabled file is written in the data directory"
    );

    drop(occupied_listener);
}

#[test]
fn the_start_up_open_with_no_listen_address_takes_no_port() {
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = build_no_remote_state();

    open_remote_listener(&mut remote_state, &router_events_sender);

    assert!(!remote_state.is_listening, "no address opens no port");
}

#[test]
fn the_start_up_open_takes_no_port_until_the_operator_has_said_yes() {
    // An address alone opens nothing: the start opens the port only when the
    // enabled file sits in the data directory.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = RemoteState {
        remote_listen_address: Some(find_free_loopback_address()),
        data_directory: Some(data_directory.clone()),
        is_listening: false,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };

    open_remote_listener(&mut remote_state, &router_events_sender);

    assert!(
        !remote_state.is_listening,
        "no token record of a yes, so no port"
    );
    assert!(
        !CertFile::resolve_certificate_file_path(&data_directory).exists(),
        "the open stopped before it made this machine's certificate"
    );
}

#[test]
fn the_start_up_open_takes_the_port_again_once_the_answer_is_written_down() {
    // The token record beside the certificate opens the port on every start after
    // the one that wrote it, with nobody asked again.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    EnabledFile {
        file_format: ENABLED_FILE_FORMAT,
        enabled_at: SystemTime::now(),
    }
    .write_to_path(&EnabledFile::resolve_enabled_file_path(&data_directory))
    .expect("the enabled file is written");
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = RemoteState {
        remote_listen_address: None,
        data_directory: Some(data_directory),
        is_listening: false,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };

    for _ in 0..MAX_ADDRESS_ATTEMPT_COUNT {
        remote_state.remote_listen_address = Some(find_free_loopback_address());
        open_remote_listener(&mut remote_state, &router_events_sender);
        if remote_state.is_listening {
            break;
        }
    }

    assert!(
        remote_state.is_listening,
        "no loopback port could be opened in {MAX_ADDRESS_ATTEMPT_COUNT} tries"
    );
    let remote_listen_address = remote_state
        .remote_listen_address
        .clone()
        .expect("the address it took");
    assert_eq!(
        TcpListener::bind(&remote_listen_address)
            .expect_err("the router is holding the address")
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
}

#[test]
fn an_address_that_cannot_be_taken_writes_no_record_of_the_answer() {
    // `enable_remote_access` binds the port before it writes the enabled file.
    // An address another socket holds is refused, and no enabled file is
    // written.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");

    let occupied_listener = TcpListener::bind("127.0.0.1:0").expect("hold a loopback address");
    let occupied_socket_address = occupied_listener
        .local_addr()
        .expect("read the held address");

    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = RemoteState {
        remote_listen_address: Some(occupied_socket_address.to_string()),
        data_directory: Some(data_directory.clone()),
        is_listening: false,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };

    let bind_error =
        TcpListener::bind(occupied_socket_address).expect_err("the address is already held");
    let enable_result = enable_remote_access(&mut remote_state, &router_events_sender);

    assert_eq!(
        enable_result,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!(
                "the remote listener could not open {occupied_socket_address}: {bind_error}"
            ),
        })
    );
    assert!(
        !remote_state.is_listening,
        "nothing is being served on an address that was never taken"
    );
    assert!(
        !EnabledFile::resolve_enabled_file_path(&data_directory).exists(),
        "the enabled file was never written"
    );

    drop(occupied_listener);
}

#[test]
fn taking_the_address_writes_the_record_and_serves_on_it() {
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");

    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = RemoteState {
        remote_listen_address: None,
        data_directory: Some(data_directory.clone()),
        is_listening: false,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };

    let enable_result = enable_remote_on_a_free_port(&mut remote_state, &router_events_sender);

    let RouterResult::RemoteEnabled {
        remote_listen_address: served_remote_listen_address,
        certificate_fingerprint,
    } = enable_result
    else {
        panic!("an address that can be taken is enabled, and got {enable_result:?}");
    };
    assert_eq!(
        served_remote_listen_address,
        remote_state
            .remote_listen_address
            .clone()
            .expect("the address it took")
    );
    let (_certificate_file, disk_certificate_fingerprint) =
        load_or_create_certificate(&data_directory).expect("this machine's certificate");
    assert_eq!(
        certificate_fingerprint, disk_certificate_fingerprint,
        "the answer names the certificate this machine now presents"
    );
    assert!(remote_state.is_listening, "the port is being served");
    assert!(
        is_remote_enabled(&data_directory),
        "the enabled file is written in the data directory"
    );
}

#[test]
fn the_status_separates_the_answer_given_from_the_port_being_open() {
    // The enabled file exists and no port is open: the status reports
    // `is_remote_access_enabled` as `true` and `is_listening` as `false`.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    EnabledFile {
        file_format: ENABLED_FILE_FORMAT,
        enabled_at: SystemTime::now(),
    }
    .write_to_path(&EnabledFile::resolve_enabled_file_path(&data_directory))
    .expect("the enabled file is written");

    let remote_state = RemoteState {
        remote_listen_address: Some("127.0.0.1:7654".to_string()),
        data_directory: Some(data_directory),
        is_listening: false,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };

    let RouterResult::RemoteStatus {
        is_remote_access_enabled,
        is_listening,
        ..
    } = build_remote_status_result(&remote_state)
    else {
        panic!("a status request is answered with a status");
    };
    assert!(is_remote_access_enabled, "the operator did say yes");
    assert!(!is_listening, "and this run is holding no port");
}

#[test]
fn the_status_names_this_machines_certificate_and_how_many_connections_it_holds() {
    // The status names the certificate fingerprint and the count of admitted
    // connections. A certificate with no enabled file reports
    // `is_remote_access_enabled` as `false`.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let (_certificate_file, certificate_fingerprint) =
        load_or_create_certificate(&data_directory).expect("this machine's certificate");
    let mut remote_state = RemoteState {
        remote_listen_address: Some("127.0.0.1:7654".to_string()),
        data_directory: Some(data_directory),
        is_listening: true,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };
    let _served_streams: Vec<TcpStream> = (0..3u64)
        .map(|remote_connection_id| {
            let (caller_stream, served_stream) = build_loopback_connection_pair();
            remote_state
                .admitted_remote_connections
                .push(AdmittedRemoteConnection {
                    token_hash: "d".repeat(64),
                    tcp_stream: caller_stream,
                    remote_connection_id,
                });
            served_stream
        })
        .collect();

    assert_eq!(
        build_remote_status_result(&remote_state),
        RouterResult::RemoteStatus {
            remote_listen_address: Some("127.0.0.1:7654".to_string()),
            is_remote_access_enabled: false,
            is_listening: true,
            certificate_fingerprint: Some(certificate_fingerprint),
            remote_connection_count: Some(3),
        }
    );
}

#[test]
fn a_bound_port_that_is_never_served_is_given_back() {
    // `enable_remote_access` binds the port, writes the enabled file, then
    // starts serving. A failed write drops the bound listener: the same address
    // binds again after the drop.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let (certificate_file, _) =
        load_or_create_certificate(&data_directory).expect("this machine's certificate");
    let remote_listen_address = find_free_loopback_address();

    let bound_listener =
        remote_listener::bind_remote_listener(remote_listen_address.clone(), &certificate_file)
            .expect("the port is taken");
    drop(bound_listener);

    // The accept thread the bind started ends once its sender drops, and
    // releases the port.
    let mut is_port_free = false;
    for _ in 0..50 {
        if TcpListener::bind(&remote_listen_address).is_ok() {
            is_port_free = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(is_port_free, "a port that was never served is free again");
}
