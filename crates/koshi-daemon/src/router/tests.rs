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

use std::net::{SocketAddr, TcpListener};
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
#[cfg(unix)]
use koshi_test_support::child_exit::wait_until_child_has_exited;
use koshi_test_support::fixtures::{
    build_test_runtime_directory, count_program_runs, write_printing_program, NO_SUCH_PROCESS_ID,
};

#[test]
fn unreadable_existing_certificate_is_kept_for_recovery() {
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let certificate_path = CertificateFile::resolve_certificate_file_path(&data_directory);
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

/// Build a session registry from `(session_id, session_name)` pairs. Each
/// session is one this router started: process id `4242`, an exit watcher,
/// and the address [`compute_socket_address`] gives under `/nowhere`. On Unix
/// nothing listens at that path. On Windows the address is the pipe name of
/// the session id alone, so a listener a test binds for the same session id
/// answers at it.
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
                    has_exit_watcher: true,
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
    let router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        taken_session_id,
        "S-quiet-lake",
    )]));

    assert!(is_session_name_taken(&router_sessions, "S-quiet-lake"));
    assert!(!is_session_name_taken(&router_sessions, "S-loud-river"));
    assert!(!is_session_name_taken(&router_sessions, "S-quiet-lak"));
    assert!(!is_session_name_taken(&router_sessions, "S-quiet-lakes"));
}

#[cfg(unix)]
#[test]
fn the_name_walk_rejects_a_name_a_starting_session_server_holds() {
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (response_sender, _response_receiver) = mpsc::channel();
    router_sessions
        .starting_session_servers
        .push(StartingSessionServer {
            session_id: SessionId::new(),
            session_name: "S-quiet-lake".to_string(),
            child_process: spawn_running_child("sleep 30"),
            ready_deadline: Instant::now() + SESSION_SERVER_READY_TIMEOUT_DURATION,
            is_ready_report_sent: Arc::new(AtomicBool::new(false)),
            response_sender,
        });

    assert!(is_session_name_taken(&router_sessions, "S-quiet-lake"));
    assert!(!is_session_name_taken(&router_sessions, "S-loud-river"));
    for mut starting_session_server in router_sessions.starting_session_servers {
        terminate_child_process(&mut starting_session_server.child_process);
    }
}

#[test]
fn the_name_walk_over_an_empty_list_takes_the_first_name_it_tries() {
    let router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let session_name = generate_name(NameKind::Session, |candidate_session_name| {
        is_session_name_taken(&router_sessions, candidate_session_name)
    });

    assert_eq!(session_name.split('-').next(), Some("S"));
    assert!(!is_session_name_taken(&router_sessions, &session_name));
}

#[test]
fn the_name_walk_hands_back_a_name_the_list_does_not_hold() {
    // With one name taken, a second walk returns a different name.
    let first_session_id = SessionId::new();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let taken_session_name = generate_name(NameKind::Session, |candidate_session_name| {
        is_session_name_taken(&router_sessions, candidate_session_name)
    });
    router_sessions.session_registry =
        build_session_registry(&[(first_session_id, &taken_session_name)]);

    let second_session_name = generate_name(NameKind::Session, |candidate_session_name| {
        is_session_name_taken(&router_sessions, candidate_session_name)
    });

    assert_ne!(second_session_name, taken_session_name);
    assert!(!is_session_name_taken(
        &router_sessions,
        &second_session_name
    ));
}

#[test]
fn a_selector_resolves_by_id() {
    let requested_session_id = SessionId::new();
    let other_session_id = SessionId::new();
    let foreign_session_id = SessionId::new();
    let absent_session_id = SessionId::new();
    let mut session_registry = build_session_registry(&[
        (requested_session_id, "S-quiet-lake"),
        (other_session_id, "S-loud-river"),
    ]);
    session_registry.insert(
        foreign_session_id,
        build_foreign_session_record(foreign_session_id, "S-quiet-lake"),
    );

    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionId(requested_session_id)
        ),
        SessionSelection::ThisUser(requested_session_id)
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionId(other_session_id)
        ),
        SessionSelection::ThisUser(other_session_id)
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionId(foreign_session_id)
        ),
        SessionSelection::OtherUser(foreign_session_id)
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionId(absent_session_id)
        ),
        SessionSelection::NotListed
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
        SessionSelection::ThisUser(requested_session_id)
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName("S-quiet".to_string())
        ),
        SessionSelection::NotListed
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName("s-quiet-lake".to_string())
        ),
        SessionSelection::NotListed
    );
    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName(String::new())
        ),
        SessionSelection::NotListed
    );
}

/// A record of a session another local user started, named `session_name`:
/// process id `0`, no exit watcher.
fn build_foreign_session_record(session_id: SessionId, session_name: &str) -> SessionRecord {
    SessionRecord {
        session_name: session_name.to_string(),
        socket_address: build_foreign_socket_address(session_id),
        process_id: 0,
        has_exit_watcher: false,
    }
}

#[test]
fn a_name_this_users_session_and_other_users_sessions_carry_resolves_to_this_users() {
    // Eight other-user sessions carry the name as well: whatever order the
    // list holds them in, this user's session wins.
    let own_session_id = SessionId::new();
    let mut session_registry = build_session_registry(&[(own_session_id, "S-quiet-lake")]);
    for _ in 0..8 {
        let foreign_session_id = SessionId::new();
        session_registry.insert(
            foreign_session_id,
            build_foreign_session_record(foreign_session_id, "S-quiet-lake"),
        );
    }

    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName("S-quiet-lake".to_string())
        ),
        SessionSelection::ThisUser(own_session_id)
    );
}

#[test]
fn a_name_several_sessions_of_this_user_carry_resolves_to_the_lowest_id() {
    // Two other-user sessions carry the name as well.
    let own_session_ids: Vec<SessionId> = (0..8).map(|_| SessionId::new()).collect();
    let mut session_registry: SessionRegistry = build_session_registry(
        &own_session_ids
            .iter()
            .map(|own_session_id| (*own_session_id, "S-quiet-lake"))
            .collect::<Vec<(SessionId, &str)>>(),
    );
    for _ in 0..2 {
        let foreign_session_id = SessionId::new();
        session_registry.insert(
            foreign_session_id,
            build_foreign_session_record(foreign_session_id, "S-quiet-lake"),
        );
    }

    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName("S-quiet-lake".to_string())
        ),
        SessionSelection::ThisUser(
            *own_session_ids
                .iter()
                .min()
                .expect("eight session ids are built")
        )
    );
}

#[test]
fn a_name_one_other_users_session_carries_resolves_to_that_session() {
    let foreign_session_id = SessionId::new();
    let mut session_registry = build_session_registry(&[(SessionId::new(), "S-loud-river")]);
    session_registry.insert(
        foreign_session_id,
        build_foreign_session_record(foreign_session_id, "S-quiet-lake"),
    );

    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName("S-quiet-lake".to_string())
        ),
        SessionSelection::OtherUser(foreign_session_id)
    );
}

#[test]
fn a_name_several_other_users_sessions_carry_and_no_session_of_this_user_carries_is_ambiguous() {
    let foreign_session_ids: Vec<SessionId> = (0..8).map(|_| SessionId::new()).collect();
    let session_registry: SessionRegistry = foreign_session_ids
        .iter()
        .map(|foreign_session_id| {
            (
                *foreign_session_id,
                build_foreign_session_record(*foreign_session_id, "S-quiet-lake"),
            )
        })
        .collect();
    let mut sorted_foreign_session_ids = foreign_session_ids.clone();
    sorted_foreign_session_ids.sort();

    assert_eq!(
        resolve_session_selector(
            &session_registry,
            &SessionSelector::SessionName("S-quiet-lake".to_string())
        ),
        SessionSelection::AmbiguousName {
            session_ids: sorted_foreign_session_ids
        }
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

    assert_eq!(
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut session_registry,
            removed_session_id,
            read_current_endpoint_file(runtime_directory.path(), removed_session_id).as_ref(),
        ),
        SessionRemoval::Removed
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
        process_id: NO_SUCH_PROCESS_ID,
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
    assert_eq!(
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut session_registry,
            removed_session_id,
            read_current_endpoint_file(runtime_directory.path(), removed_session_id).as_ref(),
        ),
        SessionRemoval::Removed
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
        process_id: NO_SUCH_PROCESS_ID,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");

    let mut session_registry = SessionRegistry::new();
    assert_eq!(
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut session_registry,
            removed_session_id,
            read_current_endpoint_file(runtime_directory.path(), removed_session_id).as_ref(),
        ),
        SessionRemoval::Removed
    );

    assert_eq!(session_registry, SessionRegistry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

#[test]
fn the_startup_over_an_empty_runtime_directory_finds_no_session() {
    let runtime_directory = build_test_runtime_directory();

    let router_sessions = create_settled_router_sessions(runtime_directory.path(), None);

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
}

#[test]
fn the_startup_drops_an_endpoint_nothing_listens_behind() {
    // The endpoint file outlived its session server. Its description finds
    // nothing listening, and the endpoint file it read is unchanged: the
    // answer removes the file and registers nothing.
    let dead_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), dead_session_id);
    EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), dead_session_id),
        connection_token: ConnectionToken::from_secret("c".repeat(64)),
        process_id: NO_SUCH_PROCESS_ID,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");

    let router_sessions = create_settled_router_sessions(runtime_directory.path(), None);

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

/// Write an endpoint file for `session_id` in `runtime_directory` in format 3,
/// which this build does not read.
fn write_unreadable_endpoint_file(runtime_directory: &Path, session_id: SessionId) -> PathBuf {
    let endpoint_path = EndpointFile::resolve_endpoint_file_path(runtime_directory, session_id);
    let endpoint_json = serde_json::json!({
        "file_format": 3,
        "socket_address": compute_socket_address(runtime_directory, session_id),
        "connection_token": "f".repeat(64),
        "process_id": 5000,
    });
    std::fs::write(&endpoint_path, endpoint_json.to_string())
        .expect("the endpoint file is written");
    endpoint_path
}

#[test]
fn the_startup_keeps_every_endpoint_file_it_cannot_read() {
    // Something listens at the private address of one session and nothing at
    // the other's. Neither is asked nor listed, both endpoint files stay, and
    // both carry the read failure as the reason they are left out.
    let live_session_id = SessionId::new();
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let live_endpoint_path =
        write_unreadable_endpoint_file(runtime_directory.path(), live_session_id);
    let silent_endpoint_path =
        write_unreadable_endpoint_file(runtime_directory.path(), silent_session_id);
    let listener = bind_test_session_listener(&compute_socket_address(
        runtime_directory.path(),
        live_session_id,
    ));

    let router_sessions = create_settled_router_sessions(runtime_directory.path(), None);

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::new(),
        "no session with an unreadable endpoint file is asked"
    );
    let mut expected_reasons = vec![
        (
            live_session_id,
            format!(
                "endpoint file {} is unreadable: format 3 is not the 2 this build reads",
                live_endpoint_path.display()
            ),
        ),
        (
            silent_session_id,
            format!(
                "endpoint file {} is unreadable: format 3 is not the 2 this build reads",
                silent_endpoint_path.display()
            ),
        ),
    ];
    expected_reasons.sort();
    assert_eq!(
        router_sessions
            .unanswered_reason_by_session_id
            .iter()
            .map(|(session_id, unanswered_session_reason)| {
                (*session_id, unanswered_session_reason.to_string())
            })
            .collect::<Vec<(SessionId, String)>>(),
        expected_reasons
    );
    assert!(
        live_endpoint_path.exists(),
        "the endpoint file of a session something listens for stays"
    );
    assert!(
        silent_endpoint_path.exists(),
        "the endpoint file of a session nothing listens for stays"
    );
    drop(listener);
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

/// The endpoint file of `session_id` in `runtime_directory` as it is on disk
/// now, or `None` when there is none: the file a caller read before it found
/// nothing listening.
fn read_current_endpoint_file(
    runtime_directory: &Path,
    session_id: SessionId,
) -> Option<EndpointFile> {
    load_session_endpoint_file(runtime_directory, session_id).expect("the endpoint file reads")
}

/// One second past [`RESTART_WINDOW_DURATION`]: the swap that wrote a file
/// this old is dead.
const PAST_RESTART_WINDOW_DURATION: Duration =
    Duration::from_secs(RESTART_WINDOW_DURATION.as_secs() + 1);

/// One second, well inside [`RESTART_WINDOW_DURATION`]: the swap that wrote a
/// file this old may still be in flight.
const INSIDE_RESTART_WINDOW_DURATION: Duration = Duration::from_secs(1);

#[test]
fn removing_a_session_takes_its_resume_and_program_files_with_it() {
    // A resume file past the window goes with the session's other files.
    let gone_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), gone_session_id);
    EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), gone_session_id),
        connection_token: ConnectionToken::from_secret("d".repeat(64)),
        process_id: NO_SUCH_PROCESS_ID,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");
    let program_file_path = ServerProgramFile::resolve_session_program_file_path(
        runtime_directory.path(),
        gone_session_id,
    );
    ServerProgramFile {
        process_id: NO_SUCH_PROCESS_ID,
        build_version: "0.6.0".to_string(),
        program_path: "/usr/local/bin/koshi".to_string(),
    }
    .write_to_path(&program_file_path)
    .expect("the program file is written");
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        gone_session_id,
        PAST_RESTART_WINDOW_DURATION,
    );

    let mut session_registry = build_session_registry(&[(gone_session_id, "S-quiet-lake")]);
    assert_eq!(
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut session_registry,
            gone_session_id,
            read_current_endpoint_file(runtime_directory.path(), gone_session_id).as_ref(),
        ),
        SessionRemoval::Removed
    );

    assert_eq!(session_registry, SessionRegistry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
    assert!(!resume_file_path.exists(), "the resume file is removed");
    assert!(!program_file_path.exists(), "the program file is removed");
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
        process_id: std::process::id(),
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        replacing_session_id,
        INSIDE_RESTART_WINDOW_DURATION,
    );

    let mut session_registry = build_session_registry(&[(replacing_session_id, "S-quiet-lake")]);
    assert_eq!(
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut session_registry,
            replacing_session_id,
            read_current_endpoint_file(runtime_directory.path(), replacing_session_id).as_ref(),
        ),
        SessionRemoval::ReplacingItsImage
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

#[cfg(target_os = "macos")]
#[test]
fn removing_a_session_whose_process_still_runs_leaves_it_and_its_files_alone() {
    // The endpoint file names this test process, and the resume file is past
    // the window.
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file_for_process(
        runtime_directory.path(),
        running_session_id,
        &compute_socket_address(runtime_directory.path(), running_session_id),
        std::process::id(),
    );
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        running_session_id,
        PAST_RESTART_WINDOW_DURATION,
    );
    let mut session_registry = build_session_registry(&[(running_session_id, "S-quiet-lake")]);

    assert_eq!(
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut session_registry,
            running_session_id,
            read_current_endpoint_file(runtime_directory.path(), running_session_id).as_ref(),
        ),
        SessionRemoval::ProcessStillRunning {
            process_id: std::process::id()
        }
    );
    assert_eq!(
        session_registry,
        build_session_registry(&[(running_session_id, "S-quiet-lake")])
    );
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), running_session_id)
            .exists(),
        "the endpoint file is left in place"
    );
    assert!(
        resume_file_path.exists(),
        "the resume file is left in place"
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn removing_a_session_whose_process_still_runs_removes_it_where_a_refused_connect_means_nothing_listens(
) {
    // The endpoint file names this test process. On Linux and Windows a
    // refused connect means nothing listens, whatever process the file names.
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file_for_process(
        runtime_directory.path(),
        running_session_id,
        &compute_socket_address(runtime_directory.path(), running_session_id),
        std::process::id(),
    );
    let mut session_registry = build_session_registry(&[(running_session_id, "S-quiet-lake")]);

    assert_eq!(
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut session_registry,
            running_session_id,
            read_current_endpoint_file(runtime_directory.path(), running_session_id).as_ref(),
        ),
        SessionRemoval::Removed
    );
    assert_eq!(session_registry, SessionRegistry::new());
    assert!(
        !EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), running_session_id)
            .exists(),
        "the endpoint file is removed"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn removing_a_session_whose_process_id_another_process_took_over_removes_it() {
    // The endpoint file names this test process, and was last written before
    // this process started: the session that wrote it is gone.
    let reused_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file_for_process(
        runtime_directory.path(),
        reused_session_id,
        &compute_socket_address(runtime_directory.path(), reused_session_id),
        std::process::id(),
    );
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), reused_session_id);
    std::fs::File::options()
        .write(true)
        .open(&endpoint_path)
        .expect("the endpoint file opens")
        .set_modified(UNIX_EPOCH + Duration::from_secs(1))
        .expect("the endpoint file is aged");
    let mut session_registry = build_session_registry(&[(reused_session_id, "S-quiet-lake")]);

    assert_eq!(
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut session_registry,
            reused_session_id,
            read_current_endpoint_file(runtime_directory.path(), reused_session_id).as_ref(),
        ),
        SessionRemoval::Removed
    );
    assert_eq!(session_registry, SessionRegistry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

#[cfg(not(target_os = "macos"))]
#[test]
fn a_probe_finding_nothing_listening_removes_the_session_while_its_process_still_runs() {
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file_for_process(
        runtime_directory.path(),
        running_session_id,
        &compute_socket_address(runtime_directory.path(), running_session_id),
        std::process::id(),
    );
    let probed_endpoint_file = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), running_session_id),
    )
    .expect("the endpoint file is read");
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        running_session_id,
        "S-quiet-lake",
    )]));

    let session_probe_verdict = apply_session_probe(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        running_session_id,
        Some(&probed_endpoint_file),
        SessionProbeOutcome::NoListener,
    );

    assert_eq!(
        session_probe_verdict,
        SessionProbeVerdict::NothingListening(SessionRemoval::Removed)
    );
    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
}

#[test]
fn a_description_whose_session_does_not_answer_ends_at_its_answer_deadline() {
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener = advertise_listening_session(runtime_directory.path(), silent_session_id);
    let description_started_at = Instant::now();

    let session_described = fetch_session_description(
        runtime_directory.path(),
        silent_session_id,
        DescribedSessionOrigin::ThisUser,
        description_started_at + Duration::from_millis(200),
    );

    assert!(
        description_started_at.elapsed() < PROMPT_ANSWER_DURATION,
        "the description ends at its deadline"
    );
    let RouterEvent::SessionDescribed {
        description_answer, ..
    } = session_described
    else {
        panic!("a description reports as SessionDescribed");
    };
    let Err(CliError::SessionAnswerTimedOut) = description_answer else {
        panic!("expected SessionAnswerTimedOut, got {description_answer:?}");
    };
    drop(session_listener);
}

#[cfg(target_os = "macos")]
#[test]
fn a_description_of_a_session_that_keeps_refusing_ends_at_its_answer_deadline() {
    // The endpoint file names this test process, written after it started.
    // The last attempt is refused, or reaches its connect with less than 1 ms
    // left and ends as timed out.
    let refusing_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file_for_process(
        runtime_directory.path(),
        refusing_session_id,
        &compute_socket_address(runtime_directory.path(), refusing_session_id),
        std::process::id(),
    );
    let description_started_at = Instant::now();

    let session_described = fetch_session_description(
        runtime_directory.path(),
        refusing_session_id,
        DescribedSessionOrigin::ThisUser,
        description_started_at + Duration::from_millis(200),
    );
    let description_duration = description_started_at.elapsed();

    assert!(
        description_duration < koshi_link::discovery::REFUSED_SESSION_RECHECK_WINDOW_DURATION,
        "the recheck ends at the answer deadline, the description took {description_duration:?}"
    );
    let RouterEvent::SessionDescribed {
        description_answer, ..
    } = session_described
    else {
        panic!("a description reports as SessionDescribed");
    };
    match description_answer {
        Err(CliError::SessionNotFound { session_name })
            if session_name == refusing_session_id.to_string() => {}
        Err(CliError::SessionAnswerTimedOut) => {}
        unexpected_answer => panic!("expected the last refusal, got {unexpected_answer:?}"),
    }
}

#[test]
fn a_description_that_ran_out_of_time_records_that_it_did_not_answer_within_5_seconds() {
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener = advertise_listening_session(runtime_directory.path(), silent_session_id);
    let described_endpoint_file = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), silent_session_id),
    )
    .expect("the endpoint file is read");
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    apply_session_description(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &build_unread_router_events_sender(),
        silent_session_id,
        &DescribedSessionOrigin::ThisUser,
        Some(&described_endpoint_file),
        Err(CliError::SessionAnswerTimedOut),
    );

    assert_eq!(
        find_unanswered_session_reason(&router_sessions, silent_session_id),
        Some("it did not answer within 5 seconds".to_string())
    );
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), silent_session_id)
            .exists(),
        "the endpoint file is left in place"
    );
    drop(session_listener);
}

#[cfg(unix)]
#[test]
fn a_process_id_converts_to_unix_only_inside_the_positive_pid_range() {
    assert_eq!(convert_to_unix_process_id(5000), Some(5000));
    assert_eq!(
        convert_to_unix_process_id(2_147_483_647),
        Some(2_147_483_647)
    );
    assert_eq!(convert_to_unix_process_id(0), None);
    assert_eq!(convert_to_unix_process_id(2_147_483_648), None);
    assert_eq!(convert_to_unix_process_id(u32::MAX), None);
}

#[cfg(unix)]
#[test]
fn adoption_skips_a_process_id_outside_the_positive_pid_range() {
    // A running child of this process: `waitpid` on process id `-1` would
    // wait for it.
    let mut running_child = spawn_running_child("sleep 30");
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    adopt_inherited_children(
        vec![0, u32::MAX],
        &mut router_sessions,
        &router_events_sender,
    );
    drop(router_events_sender);

    assert_eq!(
        (
            router_sessions.inherited_child_process_ids.clone(),
            router_sessions.unwaited_child_process_ids.clone()
        ),
        (BTreeSet::new(), BTreeSet::new()),
        "neither id is adopted"
    );
    assert_eq!(
        router_events_receiver
            .recv_timeout(NO_FURTHER_EVENT_DURATION)
            .err(),
        Some(RecvTimeoutError::Disconnected),
        "no thread holds a sender"
    );
    terminate_child_process(&mut running_child);
}

#[cfg(target_os = "macos")]
#[test]
fn a_probe_finding_nothing_listening_while_the_process_runs_keeps_the_session_and_its_files() {
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file_for_process(
        runtime_directory.path(),
        running_session_id,
        &compute_socket_address(runtime_directory.path(), running_session_id),
        std::process::id(),
    );
    let probed_endpoint_file = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), running_session_id),
    )
    .expect("the endpoint file is read");
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        running_session_id,
        "S-quiet-lake",
    )]));

    let session_probe_verdict = apply_session_probe(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        running_session_id,
        Some(&probed_endpoint_file),
        SessionProbeOutcome::NoListener,
    );

    assert_eq!(
        session_probe_verdict,
        SessionProbeVerdict::NothingListening(SessionRemoval::ProcessStillRunning {
            process_id: std::process::id()
        })
    );
    assert_eq!(
        router_sessions.session_registry,
        build_session_registry(&[(running_session_id, "S-quiet-lake")])
    );
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), running_session_id)
            .exists(),
        "the endpoint file is left in place"
    );
}

#[test]
fn a_lookup_waiting_on_a_probe_that_found_the_process_running_says_it_accepts_no_connection() {
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        running_session_id,
        "S-quiet-lake",
    )]));
    let (response_sender, response_receiver) = mpsc::channel();
    router_sessions
        .waiting_attach_lookups
        .push(build_attach_lookup_waiting_on_probe(
            SessionSelector::SessionName("S-quiet-lake".to_string()),
            running_session_id,
            response_sender,
        ));

    hand_probe_verdict_to_attach_lookups(
        runtime_directory.path(),
        &mut router_sessions,
        &build_unread_router_events_sender(),
        running_session_id,
        &SessionProbeVerdict::NothingListening(SessionRemoval::ProcessStillRunning {
            process_id: 5000,
        }),
    );

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_expected_refusal(
            "session named `S-quiet-lake` is running but did not answer: process 5000 runs but \
             accepts no connection"
        ))
    );
    assert_eq!(router_sessions.waiting_attach_lookups.len(), 0);
}

#[cfg(target_os = "macos")]
#[test]
fn a_gone_answer_while_the_process_runs_records_why_and_keeps_the_files() {
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file_for_process(
        runtime_directory.path(),
        running_session_id,
        &compute_socket_address(runtime_directory.path(), running_session_id),
        std::process::id(),
    );
    let described_endpoint_file = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), running_session_id),
    )
    .expect("the endpoint file is read");
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: running_session_id,
            described_session_origin: DescribedSessionOrigin::ThisUser,
            described_endpoint_file: Some(described_endpoint_file),
            description_answer: Err(CliError::SessionNotFound {
                session_name: running_session_id.to_string(),
            }),
        },
    );

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, running_session_id),
        Some(format!(
            "process {} runs but accepts no connection",
            std::process::id()
        ))
    );
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), running_session_id)
            .exists(),
        "the endpoint file is left in place"
    );
}

#[test]
fn the_description_scan_records_an_unreadable_endpoint_file_without_asking_the_session() {
    let unreadable_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        write_unreadable_endpoint_file(runtime_directory.path(), unreadable_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    let surveyed_session_ids = start_unlisted_session_descriptions(
        runtime_directory.path(),
        &mut router_sessions,
        &router_events_sender,
        ipc_client::list_own_sessions(runtime_directory.path())
            .expect("read the runtime directory"),
        ForeignSessionListing::default(),
    );

    assert_eq!(
        surveyed_session_ids,
        BTreeSet::from([unreadable_session_id])
    );
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::new()
    );
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, unreadable_session_id),
        Some(format!(
            "endpoint file {} is unreadable: format 3 is not the 2 this build reads",
            endpoint_path.display()
        ))
    );
    assert_eq!(
        router_events_receiver
            .recv_timeout(NO_FURTHER_EVENT_DURATION)
            .err(),
        Some(RecvTimeoutError::Timeout),
        "no thread asks the session"
    );
}

#[test]
fn the_startup_removes_a_resume_file_with_no_endpoint_file_beside_it() {
    // A resume file older than the window, with no endpoint file beside it.
    let dead_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        dead_session_id,
        PAST_RESTART_WINDOW_DURATION,
    );

    let router_sessions = create_settled_router_sessions(runtime_directory.path(), None);

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert!(
        !resume_file_path.exists(),
        "the orphan resume file is removed"
    );
}

#[test]
fn the_startup_leaves_a_resume_file_a_swap_is_still_writing_its_way_out_of() {
    // The session server has written the file and has not yet bound its new
    // socket. The startup keeps the file.
    let swapping_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        swapping_session_id,
        INSIDE_RESTART_WINDOW_DURATION,
    );

    let router_sessions = create_settled_router_sessions(runtime_directory.path(), None);

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert!(
        resume_file_path.exists(),
        "a swap in flight keeps its resume file"
    );
}

#[test]
fn the_orphan_sweep_leaves_an_old_resume_file_beside_any_endpoint_file() {
    // Both resume files are past the window. One sits beside an endpoint file
    // this build reads, and one beside an endpoint file it cannot read: both
    // stay, and so do the endpoint files.
    let readable_session_id = SessionId::new();
    let unreadable_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file(
        runtime_directory.path(),
        readable_session_id,
        &compute_socket_address(runtime_directory.path(), readable_session_id),
    );
    let unreadable_endpoint_path =
        write_unreadable_endpoint_file(runtime_directory.path(), unreadable_session_id);
    let readable_resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        readable_session_id,
        PAST_RESTART_WINDOW_DURATION,
    );
    let unreadable_resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        unreadable_session_id,
        PAST_RESTART_WINDOW_DURATION,
    );

    remove_orphan_resume_files(runtime_directory.path());

    assert!(
        readable_resume_file_path.exists(),
        "the resume file beside a readable endpoint file stays"
    );
    assert!(
        unreadable_resume_file_path.exists(),
        "the resume file beside an unreadable endpoint file stays"
    );
    assert!(unreadable_endpoint_path.exists());
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

/// The overview of a session with no tab, pane or client, named
/// `session_name` and created at `session_created_at`.
fn build_test_session_overview(
    session_id: SessionId,
    session_name: &str,
    session_created_at: SystemTime,
) -> SessionOverview {
    SessionOverview {
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
    let (release_sender, release_receiver) = mpsc::channel();
    drop(release_sender);
    spawn_session_server_answering_once_released(
        socket_address,
        session_id,
        session_name,
        session_created_at,
        release_receiver,
    )
}

/// [`spawn_session_server_answering_one_connection`], reading the caller's
/// requests only once `release_receiver` gets a message or its sender drops.
/// Until then the caller's connection is accepted and nothing is answered.
fn spawn_session_server_answering_once_released(
    socket_address: &str,
    session_id: SessionId,
    session_name: &str,
    session_created_at: SystemTime,
    release_receiver: Receiver<()>,
) -> JoinHandle<Listener> {
    let listener = Listener::bind(socket_address).expect("bind the stand-in session");
    let session_overview =
        build_test_session_overview(session_id, session_name, session_created_at);
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the router");
        let _ = release_receiver.recv();
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
                answer_result: IpcResult::Overview(session_overview),
            },
        ];
        for discovery_response in discovery_responses {
            connection
                .send(&discovery_response)
                .expect("send the scripted reply");
        }
        listener
    })
}

/// A router events sender whose receiver is dropped: every event sent on it
/// is discarded.
fn build_unread_router_events_sender() -> Sender<RouterEvent> {
    mpsc::channel().0
}

/// Write the endpoint file of `session_id` into `runtime_directory`, naming
/// `socket_address` and [`NO_SUCH_PROCESS_ID`].
fn write_test_endpoint_file(runtime_directory: &Path, session_id: SessionId, socket_address: &str) {
    write_test_endpoint_file_for_process(
        runtime_directory,
        session_id,
        socket_address,
        NO_SUCH_PROCESS_ID,
    );
}

/// Write the endpoint file of `session_id` into `runtime_directory`, naming
/// `socket_address` and `process_id`.
fn write_test_endpoint_file_for_process(
    runtime_directory: &Path,
    session_id: SessionId,
    socket_address: &str,
    process_id: u32,
) {
    EndpointFile {
        socket_address: socket_address.to_string(),
        connection_token: ConnectionToken::from_secret("f".repeat(64)),
        process_id,
    }
    .write_to_path(&EndpointFile::resolve_endpoint_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("the endpoint file is written");
}

#[test]
fn the_startup_registers_a_session_another_local_user_started() {
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

    let router_sessions = create_settled_router_sessions(
        runtime_directory.path(),
        Some(shared_sessions_directory.path()),
    );

    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            foreign_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address: foreign_socket_address,
                process_id: 0,
                has_exit_watcher: false,
            },
        )]),
    );

    session_server_thread
        .join()
        .expect("the other user's session exits");
}

#[test]
fn the_startup_leaves_out_a_shared_advert_nothing_listens_behind() {
    // The other user's session crashed and left its advert behind. The
    // startup skips it and removes none of that user's files.
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

    let router_sessions = create_settled_router_sessions(
        runtime_directory.path(),
        Some(shared_sessions_directory.path()),
    );

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert!(
        leftover_advert_path.exists(),
        "the other user's advert is left alone"
    );
}

/// The idle window the dispatcher tests run under: 50 ms.
const TEST_IDLE_EXIT_DURATION: Duration = Duration::from_millis(50);

/// The probe interval the dispatcher tests run under: 50 ms.
const TEST_LIVENESS_CHECK_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// How long a test waits for a dispatcher loop it expects to end: 5 s.
const LOOP_END_TIMEOUT_DURATION: Duration = Duration::from_secs(5);

/// How long [`settle_router_sessions`] waits for one event before it moves
/// the waiting requests on again: 10 ms.
const SETTLE_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(10);

/// How long a request the dispatcher answers without waiting may take in a
/// test: 1 s.
const PROMPT_ANSWER_DURATION: Duration = Duration::from_secs(1);

/// How long a test waits to see that no second event arrives: 500 ms, plus
/// [`CONNECT_WAIT_DURATION`](transport::CONNECT_WAIT_DURATION), the longest a
/// Windows connect to a busy pipe waits.
const NO_FURTHER_EVENT_DURATION: Duration =
    Duration::from_millis(500).saturating_add(transport::CONNECT_WAIT_DURATION);

/// Wait up to [`LOOP_END_TIMEOUT_DURATION`] for `loop_thread` to end, and hand
/// back what it returned. A loop still running after that fails the test.
fn join_dispatch_loop_thread<LoopResult>(loop_thread: JoinHandle<LoopResult>) -> LoopResult {
    let loop_end_deadline = Instant::now() + LOOP_END_TIMEOUT_DURATION;
    while !loop_thread.is_finished() {
        assert!(
            Instant::now() < loop_end_deadline,
            "the dispatcher loop is still running after {LOOP_END_TIMEOUT_DURATION:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    loop_thread
        .join()
        .expect("the loop thread ended without a panic")
}

/// Run the dispatcher on a thread of its own over `session_registry`, with
/// `router_events` queued in order before it starts, the idle window
/// [`TEST_IDLE_EXIT_DURATION`], and `liveness_check_interval` between probes.
/// Hands back how the loop ended and the list it ended with, through
/// [`join_dispatch_loop_thread`].
fn run_queued_dispatch_loop(
    runtime_directory: &Path,
    session_registry: SessionRegistry,
    router_events: Vec<RouterEvent>,
    liveness_check_interval: Duration,
) -> (RouterExit, SessionRegistry) {
    let held_runtime_directory = runtime_directory.to_path_buf();
    let loop_thread = std::thread::spawn(move || {
        let (router_events_sender, router_events_receiver) = mpsc::channel();
        for router_event in router_events {
            router_events_sender
                .send(router_event)
                .expect("the event is queued");
        }
        let mut router_sessions = RouterSessions::from_session_registry(session_registry);
        let router_exit = run_dispatch_loop(
            &held_runtime_directory,
            None,
            &build_test_executable_watch(),
            None,
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            liveness_check_interval,
            &mut router_sessions,
            &mut build_no_remote_state(),
        );
        (router_exit, router_sessions.session_registry)
    });
    join_dispatch_loop_thread(loop_thread)
}

/// Run the dispatcher on a thread of its own over `session_registry`, with
/// the idle window [`TEST_IDLE_EXIT_DURATION`] and the probe interval
/// [`TEST_LIVENESS_CHECK_INTERVAL_DURATION`]. Hands back a sender that queues
/// events for it, and the thread, which ends with how the loop ended and the
/// list it ended with.
fn spawn_dispatch_loop(
    runtime_directory: &Path,
    session_registry: SessionRegistry,
) -> (
    Sender<RouterEvent>,
    JoinHandle<(RouterExit, SessionRegistry)>,
) {
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let test_router_events_sender = router_events_sender.clone();
    let held_runtime_directory = runtime_directory.to_path_buf();
    let loop_thread = std::thread::spawn(move || {
        let mut router_sessions = RouterSessions::from_session_registry(session_registry);
        let router_exit = run_dispatch_loop(
            &held_runtime_directory,
            None,
            &build_test_executable_watch(),
            None,
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            TEST_LIVENESS_CHECK_INTERVAL_DURATION,
            &mut router_sessions,
            &mut build_no_remote_state(),
        );
        (router_exit, router_sessions.session_registry)
    });
    (test_router_events_sender, loop_thread)
}

/// A watch of this test binary, which a test hands the loop as the program a
/// restart would start. No test here starts it.
fn build_test_executable_watch() -> Arc<ExecutableWatch> {
    Arc::new(ExecutableWatch::new(
        std::env::current_exe().expect("this test binary's own path"),
        BUILD_VERSION,
    ))
}

/// A watch of a program file that does not exist: no connection it checks
/// runs a version read.
fn build_unread_executable_watch() -> Arc<ExecutableWatch> {
    Arc::new(ExecutableWatch::new(
        PathBuf::from("/home/user/absent/koshi"),
        BUILD_VERSION,
    ))
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

/// Whether the dispatcher waits on nothing: no description asked and not
/// answered, no probe running, no request waiting, and no session server
/// starting.
fn is_nothing_in_flight(router_sessions: &RouterSessions) -> bool {
    router_sessions
        .description_in_flight_by_session_id
        .is_empty()
        && router_sessions.probing_session_ids.is_empty()
        && router_sessions.waiting_attach_lookups.is_empty()
        && router_sessions.waiting_remote_locates.is_empty()
        && router_sessions.queued_session_creations.is_empty()
        && router_sessions.starting_session_servers.is_empty()
}

/// Do what the dispatcher loop does, on the test thread, until `is_settled`
/// holds for `router_sessions`: move every waiting request on, then serve the
/// next event a started thread reports on `router_events_receiver`. Every
/// event must leave the loop running.
///
/// Fails the test when `is_settled` does not hold within
/// [`SESSION_DISCOVERY_TIMEOUT_DURATION`] plus [`LOOP_END_TIMEOUT_DURATION`].
fn settle_router_sessions(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    remote_state: &mut RemoteState,
    router_events_sender: &Sender<RouterEvent>,
    router_events_receiver: &Receiver<RouterEvent>,
    is_settled: impl Fn(&RouterSessions) -> bool,
) {
    let settle_deadline =
        Instant::now() + SESSION_DISCOVERY_TIMEOUT_DURATION + LOOP_END_TIMEOUT_DURATION;
    loop {
        advance_waiting_requests(
            runtime_directory,
            None,
            router_sessions,
            remote_state,
            router_events_sender,
            Instant::now(),
        );
        if is_settled(router_sessions) {
            return;
        }
        assert!(
            Instant::now() < settle_deadline,
            "the router sessions did not settle in time"
        );
        let Ok(router_event) = router_events_receiver.recv_timeout(SETTLE_POLL_INTERVAL_DURATION)
        else {
            continue;
        };
        serve_router_event(
            runtime_directory,
            None,
            &build_test_executable_watch(),
            None,
            router_sessions,
            remote_state,
            router_events_sender,
            router_event,
        );
    }
}

/// The sessions [`create_router_sessions`] starts with, once every
/// description it started has answered and the answer has been applied.
fn create_settled_router_sessions(
    runtime_directory: &Path,
    shared_sessions_base_directory: Option<&Path>,
) -> RouterSessions {
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let mut router_sessions = create_router_sessions(
        runtime_directory,
        shared_sessions_base_directory,
        &router_events_sender,
    );
    settle_router_sessions(
        runtime_directory,
        &mut router_sessions,
        &mut build_no_remote_state(),
        &router_events_sender,
        &router_events_receiver,
        is_nothing_in_flight,
    );
    router_sessions
}

/// Serve `router_event` against `router_sessions`, then settle them through
/// [`settle_router_sessions`] until nothing is in flight.
fn serve_router_event_and_settle(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    router_event: RouterEvent,
) {
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let mut remote_state = build_no_remote_state();
    serve_router_event(
        runtime_directory,
        None,
        &build_test_executable_watch(),
        None,
        router_sessions,
        &mut remote_state,
        &router_events_sender,
        router_event,
    );
    settle_router_sessions(
        runtime_directory,
        router_sessions,
        &mut remote_state,
        &router_events_sender,
        &router_events_receiver,
        is_nothing_in_flight,
    );
}

/// Take one attach lookup for `session_selector` as the dispatcher takes it,
/// settle `router_sessions` until it is answered, and hand back the answer.
fn run_attach_lookup(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    shared_sessions_base_directory: Option<&Path>,
    session_selector: SessionSelector,
) -> RouterResult {
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let (response_sender, response_receiver) = mpsc::channel();
    let foreign_session_listing = match list_foreign_sessions_for_lookup(
        shared_sessions_base_directory,
        runtime_directory,
        &router_sessions.session_registry,
        &session_selector,
    ) {
        Ok(foreign_session_listing) => foreign_session_listing,
        Err(foreign_session_lookup_error) => {
            return build_refused_result(foreign_session_lookup_error.to_string())
        }
    };
    start_attach_lookup(
        runtime_directory,
        router_sessions,
        &router_events_sender,
        foreign_session_listing,
        session_selector,
        response_sender,
    );
    settle_router_sessions(
        runtime_directory,
        router_sessions,
        &mut build_no_remote_state(),
        &router_events_sender,
        &router_events_receiver,
        |router_sessions| router_sessions.waiting_attach_lookups.is_empty(),
    );
    response_receiver
        .try_recv()
        .expect("the lookup is answered")
}

/// Record a description of the session `session_id` asked at `asked_at` whose
/// answer is not sent, with no thread behind it, as one of this user's.
fn insert_description_in_flight(
    router_sessions: &mut RouterSessions,
    session_id: SessionId,
    asked_at: Instant,
) {
    router_sessions.description_in_flight_by_session_id.insert(
        session_id,
        DescriptionInFlight {
            asked_at,
            is_other_user_session: false,
            is_answer_sent: Arc::new(AtomicBool::new(false)),
        },
    );
}

/// The sessions a description is in flight for.
fn list_describing_session_ids(router_sessions: &RouterSessions) -> BTreeSet<SessionId> {
    router_sessions
        .description_in_flight_by_session_id
        .keys()
        .copied()
        .collect()
}

/// The moment [`SESSION_DISCOVERY_TIMEOUT_DURATION`] before now. A
/// description asked then is waited for no more.
fn compute_expired_asked_at() -> Instant {
    Instant::now()
        .checked_sub(SESSION_DISCOVERY_TIMEOUT_DURATION)
        .expect("this clock reaches back 5 seconds")
}

#[test]
fn an_idle_window_that_passes_with_no_session_running_ends_the_loop() {
    let runtime_directory = build_test_runtime_directory();

    let (router_exit, remaining_session_registry) = run_queued_dispatch_loop(
        runtime_directory.path(),
        SessionRegistry::new(),
        Vec::new(),
        TEST_LIVENESS_CHECK_INTERVAL_DURATION,
    );

    assert_eq!(router_exit, RouterExit::Idle);
    assert_eq!(remaining_session_registry, SessionRegistry::new());
}

#[test]
fn a_request_inside_the_idle_window_is_served_and_the_loop_goes_on() {
    // The loop answers a request queued before it starts, then ends on the
    // idle window.
    let runtime_directory = build_test_runtime_directory();
    let (response_sender, response_receiver) = mpsc::channel();
    let missing_session_id = SessionId::new();

    let (router_exit, remaining_session_registry) = run_queued_dispatch_loop(
        runtime_directory.path(),
        SessionRegistry::new(),
        vec![RouterEvent::Request {
            request_kind: RouterRequestKind::AttachLookup {
                session_selector: SessionSelector::SessionId(missing_session_id),
            },
            response_sender,
        }],
        TEST_LIVENESS_CHECK_INTERVAL_DURATION,
    );

    assert_eq!(
        response_receiver
            .try_recv()
            .expect("the loop answered the request"),
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: format!("no session {missing_session_id} is running"),
        })
    );
    assert_eq!(router_exit, RouterExit::Idle);
    assert_eq!(remaining_session_registry, SessionRegistry::new());
}

#[test]
fn a_due_restart_ends_the_loop_for_the_swap() {
    // `RestartDue` ends the loop with `RouterExit::Restart`, and the
    // session list keeps every entry.
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();

    let (router_exit, remaining_session_registry) = run_queued_dispatch_loop(
        runtime_directory.path(),
        build_session_registry(&[(running_session_id, "S-quiet-lake")]),
        vec![RouterEvent::RestartDue],
        TEST_LIVENESS_CHECK_INTERVAL_DURATION,
    );

    assert_eq!(router_exit, RouterExit::Restart);
    assert_eq!(
        remaining_session_registry,
        build_session_registry(&[(running_session_id, "S-quiet-lake")])
    );
}

#[test]
fn a_running_session_keeps_the_loop_alive_past_the_idle_window() {
    // The loop keeps serving past the idle window while the list holds a
    // session.
    let running_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (test_router_events_sender, loop_thread) = spawn_dispatch_loop(
        runtime_directory.path(),
        build_session_registry(&[(running_session_id, "S-quiet-lake")]),
    );

    std::thread::sleep(TEST_IDLE_EXIT_DURATION * 5);
    assert!(
        !loop_thread.is_finished(),
        "the loop is still serving while a session is running"
    );

    test_router_events_sender
        .send(RouterEvent::ChildExited(running_session_id))
        .expect("the exit is queued");
    let (router_exit, remaining_session_registry) = join_dispatch_loop_thread(loop_thread);

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
    let (test_router_events_sender, loop_thread) = spawn_dispatch_loop(
        runtime_directory.path(),
        build_session_registry(&[
            (removed_session_id, "S-quiet-lake"),
            (retained_session_id, "S-loud-river"),
        ]),
    );

    test_router_events_sender
        .send(RouterEvent::ChildExited(removed_session_id))
        .expect("the exit is queued");
    std::thread::sleep(TEST_IDLE_EXIT_DURATION * 5);
    assert!(
        !loop_thread.is_finished(),
        "the loop keeps serving while one session is listed"
    );

    test_router_events_sender
        .send(RouterEvent::ChildExited(retained_session_id))
        .expect("the second exit is queued");
    let (router_exit, remaining_session_registry) = join_dispatch_loop_thread(loop_thread);

    assert_eq!(router_exit, RouterExit::Idle);
    assert_eq!(remaining_session_registry, SessionRegistry::new());
}

#[test]
fn a_lookup_waiting_on_a_silent_session_leaves_the_loop_serving_other_requests() {
    // The session takes the description's connection and never answers. The
    // lookup by its id waits for that answer. A status request sent after the
    // lookup is answered at once, the idle window passes without ending the
    // loop, and the lookup is answered once 5 seconds pass.
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener = advertise_listening_session(runtime_directory.path(), silent_session_id);
    let (test_router_events_sender, loop_thread) =
        spawn_dispatch_loop(runtime_directory.path(), SessionRegistry::new());
    let (lookup_sender, lookup_receiver) = mpsc::channel();
    let (status_sender, status_receiver) = mpsc::channel();

    let lookup_sent_at = Instant::now();
    test_router_events_sender
        .send(RouterEvent::Request {
            request_kind: RouterRequestKind::AttachLookup {
                session_selector: SessionSelector::SessionId(silent_session_id),
            },
            response_sender: lookup_sender,
        })
        .expect("the lookup is queued");
    test_router_events_sender
        .send(RouterEvent::Request {
            request_kind: RouterRequestKind::RemoteStatus,
            response_sender: status_sender,
        })
        .expect("the status request is queued");

    assert_eq!(
        status_receiver
            .recv_timeout(PROMPT_ANSWER_DURATION)
            .expect("the status is answered while the lookup waits"),
        RouterResult::RemoteStatus {
            remote_listen_address: None,
            is_remote_access_enabled: false,
            is_listening: false,
            certificate_fingerprint: None,
            remote_connection_count: 0,
        }
    );
    assert_eq!(
        lookup_receiver.try_recv(),
        Err(mpsc::TryRecvError::Empty),
        "the lookup still waits"
    );
    assert_eq!(
        lookup_receiver
            .recv_timeout(SESSION_DISCOVERY_TIMEOUT_DURATION + LOOP_END_TIMEOUT_DURATION)
            .expect("the lookup is answered"),
        build_expected_refusal(&format!(
            "session {silent_session_id} is running but did not answer: it did not answer \
             within 5 seconds"
        ))
    );
    assert!(
        lookup_sent_at.elapsed() >= SESSION_DISCOVERY_TIMEOUT_DURATION,
        "the lookup waited 5 seconds for the description"
    );
    drop(session_listener);
    let (router_exit, remaining_session_registry) = join_dispatch_loop_thread(loop_thread);
    assert_eq!(router_exit, RouterExit::Idle);
    assert_eq!(remaining_session_registry, SessionRegistry::new());
}

#[test]
fn a_lookup_for_a_session_the_list_does_not_hold_is_refused_by_name() {
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
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
        process_id: NO_SUCH_PROCESS_ID,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");
    let mut session_registry = build_session_registry(&[(dead_session_id, "S-quiet-lake")]);
    session_registry
        .get_mut(&dead_session_id)
        .expect("the session is listed")
        .socket_address = compute_socket_address(runtime_directory.path(), dead_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(session_registry);

    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionId(dead_session_id),
    );

    assert_eq!(
        router_result,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: format!("no session {dead_session_id} is running"),
        })
    );
    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
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
fn the_startup_keeps_the_files_of_a_session_it_cannot_read_a_version_from() {
    let live_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), live_session_id);
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), live_session_id);
    EndpointFile {
        socket_address: socket_address.clone(),
        connection_token: ConnectionToken::from_secret("f".repeat(64)),
        process_id: NO_SUCH_PROCESS_ID,
    }
    .write_to_path(&endpoint_path)
    .expect("the endpoint file is written");
    let session_server_thread = spawn_version_mismatched_session_server(&socket_address);

    let router_sessions = create_settled_router_sessions(runtime_directory.path(), None);
    session_server_thread
        .join()
        .expect("the stand-in session ended");

    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::new(),
        "a session that could not describe itself is not listed"
    );
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, live_session_id),
        Some(format!(
            "IPC unavailable: the session settled on protocol version {}, which is outside the \
             {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION} this koshi asked for",
            PROTOCOL_VERSION + 1
        ))
    );
    assert!(
        endpoint_path.exists(),
        "a session that is still bound keeps its endpoint file"
    );
}

/// A stand-in session server bound at `socket_address`. It never calls
/// `accept`: a probe connects to the bound address and closes at once, and a
/// description waits for an answer that never comes.
///
/// The caller keeps the returned listener bound for as long as the test needs
/// the session.
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
    let mut router_sessions = RouterSessions::from_session_registry(session_registry);

    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
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
        router_sessions.session_registry,
        SessionRegistry::from([(
            live_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: 4242,
                has_exit_watcher: true,
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
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let lookup_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
    );

    assert_eq!(
        lookup_result,
        RouterResult::Found(SessionAddress {
            session_id: unlisted_session_id,
            session_name: "S-quiet-lake".to_string(),
            socket_address: socket_address.clone(),
            process_id: NO_SUCH_PROCESS_ID,
        })
    );
    let listener = session_server_thread
        .join()
        .expect("the stand-in session ended");
    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            unlisted_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: NO_SUCH_PROCESS_ID,
                has_exit_watcher: false,
            },
        )]),
        "no watcher takes a process id no running process holds"
    );
    drop(listener);
}

#[test]
fn a_lookup_by_a_name_another_users_session_carries_waits_for_this_users_session_to_describe_itself(
) {
    // This user's unlisted session answers with the same name: the lookup
    // finds it, and the other user's session is never probed.
    let unlisted_session_id = SessionId::new();
    let foreign_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), unlisted_session_id);
    let session_server_thread = spawn_session_server_answering_one_connection(
        &socket_address,
        unlisted_session_id,
        "S-quiet-lake",
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    );
    write_test_endpoint_file(
        runtime_directory.path(),
        unlisted_session_id,
        &socket_address,
    );
    let mut session_registry = SessionRegistry::new();
    session_registry.insert(
        foreign_session_id,
        build_foreign_session_record(foreign_session_id, "S-quiet-lake"),
    );
    let mut router_sessions = RouterSessions::from_session_registry(session_registry);

    let lookup_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
    );

    assert_eq!(
        lookup_result,
        RouterResult::Found(SessionAddress {
            session_id: unlisted_session_id,
            session_name: "S-quiet-lake".to_string(),
            socket_address: socket_address.clone(),
            process_id: NO_SUCH_PROCESS_ID,
        })
    );
    assert_eq!(
        router_sessions.session_registry.get(&foreign_session_id),
        Some(&build_foreign_session_record(
            foreign_session_id,
            "S-quiet-lake"
        )),
        "the other user's session stays listed"
    );
    drop(
        session_server_thread
            .join()
            .expect("the stand-in session ended"),
    );
}

#[test]
fn a_lookup_by_a_name_several_other_users_sessions_carry_asks_for_the_session_id() {
    let first_foreign_session_id = SessionId::new();
    let second_foreign_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut session_registry = SessionRegistry::new();
    for foreign_session_id in [first_foreign_session_id, second_foreign_session_id] {
        session_registry.insert(
            foreign_session_id,
            build_foreign_session_record(foreign_session_id, "S-quiet-lake"),
        );
    }
    let mut router_sessions = RouterSessions::from_session_registry(session_registry.clone());
    let lower_session_id = first_foreign_session_id.min(second_foreign_session_id);
    let higher_session_id = first_foreign_session_id.max(second_foreign_session_id);

    let lookup_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
    );

    assert_eq!(
        lookup_result,
        build_expected_refusal(&format!(
            "2 sessions other local users started carry this name: {lower_session_id}, \
             {higher_session_id}; attach by session id"
        ))
    );
    assert_eq!(router_sessions.session_registry, session_registry);
    assert!(is_nothing_in_flight(&router_sessions));
}

#[test]
fn the_startup_asks_every_session_without_waiting_for_an_answer() {
    // The session takes the description's connection and never answers. The
    // startup hands back the empty list at once, with the session asked.
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener = advertise_listening_session(runtime_directory.path(), silent_session_id);

    let (startup_sender, startup_receiver) = mpsc::channel();
    let startup_runtime_directory = runtime_directory.path().to_path_buf();
    std::thread::spawn(move || {
        let _ = startup_sender.send(create_router_sessions(
            &startup_runtime_directory,
            None,
            &build_unread_router_events_sender(),
        ));
    });

    let router_sessions = startup_receiver
        .recv_timeout(PROMPT_ANSWER_DURATION)
        .expect("the startup does not wait for the description");
    assert_eq!(
        router_sessions
            .description_in_flight_by_session_id
            .keys()
            .copied()
            .collect::<Vec<SessionId>>(),
        vec![silent_session_id]
    );
    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    drop(session_listener);
}

#[test]
fn a_session_asked_5_seconds_ago_reads_as_not_answering_and_is_not_asked_again() {
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener = advertise_listening_session(runtime_directory.path(), silent_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let expired_asked_at = compute_expired_asked_at();
    insert_description_in_flight(&mut router_sessions, silent_session_id, expired_asked_at);
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    let surveyed_session_ids = start_unlisted_session_descriptions(
        runtime_directory.path(),
        &mut router_sessions,
        &router_events_sender,
        ipc_client::list_own_sessions(runtime_directory.path())
            .expect("read the runtime directory"),
        ForeignSessionListing::default(),
    );

    assert_eq!(surveyed_session_ids, BTreeSet::from([silent_session_id]));
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::from([silent_session_id]),
        "the session is not asked again"
    );
    assert_eq!(
        router_sessions.description_in_flight_by_session_id[&silent_session_id].asked_at,
        expired_asked_at
    );
    assert_eq!(
        router_events_receiver
            .recv_timeout(NO_FURTHER_EVENT_DURATION)
            .err(),
        Some(RecvTimeoutError::Timeout),
        "no thread asks the session"
    );
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, silent_session_id),
        Some("it did not answer within 5 seconds".to_string())
    );
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), silent_session_id)
            .exists(),
        "a session that is still bound keeps its endpoint file"
    );
    drop(session_listener);
}

#[test]
fn a_session_still_being_described_is_not_asked_again_until_its_answer_is_applied() {
    // The stand-in takes one connection and answers once released. A second
    // ask while the first runs opens no second connection: once the stand-in's
    // listener is dropped, no second answer arrives.
    let described_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), described_session_id);
    let (release_sender, release_receiver) = mpsc::channel();
    let session_server_thread = spawn_session_server_answering_once_released(
        &socket_address,
        described_session_id,
        "S-quiet-lake",
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        release_receiver,
    );
    write_test_endpoint_file(
        runtime_directory.path(),
        described_session_id,
        &socket_address,
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let ask_session = |router_sessions: &mut RouterSessions| {
        start_session_description(
            runtime_directory.path(),
            router_sessions,
            &router_events_sender,
            described_session_id,
            DescribedSessionOrigin::ThisUser,
        );
    };

    ask_session(&mut router_sessions);
    let first_asked_at =
        router_sessions.description_in_flight_by_session_id[&described_session_id].asked_at;
    ask_session(&mut router_sessions);

    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::from([described_session_id])
    );
    assert_eq!(
        router_sessions.description_in_flight_by_session_id[&described_session_id].asked_at,
        first_asked_at
    );
    drop(release_sender);
    let described_event = router_events_receiver
        .recv_timeout(LOOP_END_TIMEOUT_DURATION)
        .expect("the answer is reported");
    drop(
        session_server_thread
            .join()
            .expect("the stand-in session ended"),
    );
    assert_eq!(
        router_events_receiver
            .recv_timeout(NO_FURTHER_EVENT_DURATION)
            .err(),
        Some(RecvTimeoutError::Timeout),
        "one description ran"
    );
    serve_router_event(
        runtime_directory.path(),
        None,
        &build_test_executable_watch(),
        None,
        &mut router_sessions,
        &mut build_no_remote_state(),
        &router_events_sender,
        described_event,
    );
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::new()
    );
    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            described_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: NO_SUCH_PROCESS_ID,
                has_exit_watcher: false,
            },
        )])
    );

    ask_session(&mut router_sessions);

    assert_eq!(
        router_sessions
            .description_in_flight_by_session_id
            .keys()
            .copied()
            .collect::<Vec<SessionId>>(),
        vec![described_session_id],
        "an applied answer lets the session be asked again"
    );
}

/// Advertise `session_id` in `runtime_directory` with an endpoint file and a
/// listener bound at its address, and hand back that listener.
fn advertise_listening_session(runtime_directory: &Path, session_id: SessionId) -> Listener {
    let socket_address = compute_socket_address(runtime_directory, session_id);
    let session_listener = bind_test_session_listener(&socket_address);
    write_test_endpoint_file(runtime_directory, session_id, &socket_address);
    session_listener
}

/// The refusal [`build_refused_result`] gives for `refusal_message`: an
/// [`IpcErrorCode::RequestFailed`] error carrying it.
fn build_expected_refusal(refusal_message: &str) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::RequestFailed,
        message: refusal_message.to_string(),
    })
}

#[test]
fn a_lookup_by_id_for_a_listening_session_that_does_not_answer_says_so() {
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener = advertise_listening_session(runtime_directory.path(), silent_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    insert_description_in_flight(
        &mut router_sessions,
        silent_session_id,
        compute_expired_asked_at(),
    );

    let lookup_started_at = Instant::now();
    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionId(silent_session_id),
    );

    assert!(
        lookup_started_at.elapsed() < PROMPT_ANSWER_DURATION,
        "a session asked 5 seconds ago is not waited for again"
    );
    assert_eq!(
        router_result,
        build_expected_refusal(&format!(
            "session {silent_session_id} is running but did not answer: it did not answer \
             within 5 seconds"
        ))
    );
    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    drop(session_listener);
}

#[test]
fn a_gone_answer_after_the_endpoint_file_was_rewritten_asks_the_session_again() {
    // The description read one endpoint file and found nothing listening. The
    // session has since bound again and written a new endpoint file: it is
    // asked again, and that answer registers it.
    let rebound_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), rebound_session_id);
    let session_server_thread = spawn_session_server_answering_one_connection(
        &socket_address,
        rebound_session_id,
        "S-quiet-lake",
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    );
    write_test_endpoint_file(
        runtime_directory.path(),
        rebound_session_id,
        &socket_address,
    );
    let described_endpoint_file = EndpointFile {
        socket_address: socket_address.clone(),
        connection_token: ConnectionToken::from_secret("a".repeat(64)),
        process_id: NO_SUCH_PROCESS_ID,
    };
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: rebound_session_id,
            described_session_origin: DescribedSessionOrigin::ThisUser,
            described_endpoint_file: Some(described_endpoint_file),
            description_answer: Err(CliError::SessionNotFound {
                session_name: rebound_session_id.to_string(),
            }),
        },
    );

    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            rebound_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: NO_SUCH_PROCESS_ID,
                has_exit_watcher: false,
            },
        )])
    );
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), rebound_session_id)
            .exists(),
        "the endpoint file of the session that bound again is left in place"
    );
    drop(
        session_server_thread
            .join()
            .expect("the stand-in session ended"),
    );
}

#[test]
fn a_gone_answer_with_the_endpoint_file_unchanged_removes_every_file_the_session_left() {
    let gone_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), gone_session_id);
    let gone_endpoint_file = EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), gone_session_id),
        connection_token: ConnectionToken::from_secret("a".repeat(64)),
        process_id: NO_SUCH_PROCESS_ID,
    };
    gone_endpoint_file
        .write_to_path(&endpoint_path)
        .expect("the endpoint file is written");
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        gone_session_id,
        PAST_RESTART_WINDOW_DURATION,
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: gone_session_id,
            described_session_origin: DescribedSessionOrigin::ThisUser,
            described_endpoint_file: Some(gone_endpoint_file),
            description_answer: Err(CliError::SessionNotFound {
                session_name: gone_session_id.to_string(),
            }),
        },
    );

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, gone_session_id),
        None
    );
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
    assert!(!resume_file_path.exists(), "the resume file is removed");
}

#[test]
fn an_answer_applied_after_the_endpoint_file_was_removed_mid_swap_says_it_is_restarting() {
    // The old image removed its endpoint file on its way out, and its resume
    // file is inside the window: the new image has not bound yet.
    let replacing_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        replacing_session_id,
        INSIDE_RESTART_WINDOW_DURATION,
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: replacing_session_id,
            described_session_origin: DescribedSessionOrigin::ThisUser,
            described_endpoint_file: None,
            description_answer: Err(CliError::SessionNotFound {
                session_name: replacing_session_id.to_string(),
            }),
        },
    );

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, replacing_session_id),
        Some("it is restarting".to_string())
    );
    assert!(
        resume_file_path.exists(),
        "the resume file of the swap in flight is left in place"
    );
}

#[test]
fn an_answer_applied_after_the_endpoint_file_was_removed_clears_what_the_session_left() {
    // The session answered and then ended: its endpoint file is gone, and an
    // old resume file is left.
    let ended_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let resume_file_path = build_aged_resume_file(
        runtime_directory.path(),
        ended_session_id,
        PAST_RESTART_WINDOW_DURATION,
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: ended_session_id,
            described_session_origin: DescribedSessionOrigin::ThisUser,
            described_endpoint_file: None,
            description_answer: Ok(build_test_session_overview(
                ended_session_id,
                "S-quiet-lake",
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )),
        },
    );

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, ended_session_id),
        None
    );
    assert!(!resume_file_path.exists(), "the old resume file is removed");
}

#[test]
fn a_lookup_by_id_for_an_unknown_session_is_not_found_at_once_while_another_does_not_answer() {
    // One advertised session was asked just now and has not answered. A lookup
    // naming another id does not wait for it, and gets the plain refusal.
    let silent_session_id = SessionId::new();
    let unknown_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener = advertise_listening_session(runtime_directory.path(), silent_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    insert_description_in_flight(&mut router_sessions, silent_session_id, Instant::now());

    let lookup_started_at = Instant::now();
    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionId(unknown_session_id),
    );

    assert!(
        lookup_started_at.elapsed() < PROMPT_ANSWER_DURATION,
        "the lookup does not wait for another session's description"
    );
    assert_eq!(
        router_result,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: format!("no session {unknown_session_id} is running"),
        })
    );
    drop(session_listener);
}

#[test]
fn a_lookup_by_name_while_sessions_do_not_answer_counts_them() {
    let first_silent_session_id = SessionId::new();
    let second_silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let first_session_listener =
        advertise_listening_session(runtime_directory.path(), first_silent_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    insert_description_in_flight(
        &mut router_sessions,
        first_silent_session_id,
        compute_expired_asked_at(),
    );
    let lookup_quiet_lake = |router_sessions: &mut RouterSessions| {
        run_attach_lookup(
            runtime_directory.path(),
            router_sessions,
            None,
            SessionSelector::SessionName("S-quiet-lake".to_string()),
        )
    };

    assert_eq!(
        lookup_quiet_lake(&mut router_sessions),
        build_expected_refusal(
            "no session named `S-quiet-lake` answered; 1 running session did not answer, so \
             its name is unknown"
        )
    );

    let second_session_listener =
        advertise_listening_session(runtime_directory.path(), second_silent_session_id);
    insert_description_in_flight(
        &mut router_sessions,
        second_silent_session_id,
        compute_expired_asked_at(),
    );

    assert_eq!(
        lookup_quiet_lake(&mut router_sessions),
        build_expected_refusal(
            "no session named `S-quiet-lake` answered; 2 running sessions did not answer, so \
             their names are unknown"
        )
    );
    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    drop(first_session_listener);
    drop(second_session_listener);
}

#[test]
fn a_lookup_by_id_for_a_session_on_another_protocol_version_names_the_failure() {
    let mismatched_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), mismatched_session_id);
    let session_server_thread = spawn_version_mismatched_session_server(&socket_address);
    write_test_endpoint_file(
        runtime_directory.path(),
        mismatched_session_id,
        &socket_address,
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionId(mismatched_session_id),
    );
    session_server_thread
        .join()
        .expect("the stand-in session ended");

    assert_eq!(
        router_result,
        build_expected_refusal(&format!(
            "session {mismatched_session_id} is running but did not answer: IPC unavailable: the \
             session settled on protocol version {}, which is outside the {MIN_PROTOCOL_VERSION} \
             to {PROTOCOL_VERSION} this koshi asked for",
            PROTOCOL_VERSION + 1
        ))
    );
    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
}

#[test]
fn a_lookup_by_id_for_a_listening_session_with_an_unreadable_endpoint_file_names_the_failure() {
    let unreadable_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        write_unreadable_endpoint_file(runtime_directory.path(), unreadable_session_id);
    let session_listener = bind_test_session_listener(&compute_socket_address(
        runtime_directory.path(),
        unreadable_session_id,
    ));
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionId(unreadable_session_id),
    );

    assert_eq!(
        router_result,
        build_expected_refusal(&format!(
            "session {unreadable_session_id} is running but did not answer: endpoint file {} is \
             unreadable: format 3 is not the 2 this build reads",
            endpoint_path.display()
        ))
    );
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::new(),
        "a session with an unreadable endpoint file is not asked"
    );
    assert!(endpoint_path.exists(), "the endpoint file is left in place");
    drop(session_listener);
}

#[test]
fn a_lookup_by_id_for_an_advertised_session_replacing_its_image_says_it_is_restarting() {
    let replacing_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file(
        runtime_directory.path(),
        replacing_session_id,
        &compute_socket_address(runtime_directory.path(), replacing_session_id),
    );
    build_aged_resume_file(
        runtime_directory.path(),
        replacing_session_id,
        INSIDE_RESTART_WINDOW_DURATION,
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionId(replacing_session_id),
    );

    assert_eq!(
        router_result,
        build_expected_refusal(&format!(
            "session {replacing_session_id} is running but did not answer: it is restarting"
        ))
    );
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), replacing_session_id)
            .exists(),
        "the endpoint file is left in place"
    );
}

#[test]
fn a_lookup_by_name_for_a_listed_session_replacing_its_image_says_it_is_restarting() {
    let replacing_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    build_aged_resume_file(
        runtime_directory.path(),
        replacing_session_id,
        INSIDE_RESTART_WINDOW_DURATION,
    );
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        replacing_session_id,
        "S-quiet-lake",
    )]));

    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
    );

    assert_eq!(
        router_result,
        build_expected_refusal(
            "session named `S-quiet-lake` is running but did not answer: it is restarting"
        )
    );
    assert_eq!(
        router_sessions.session_registry,
        build_session_registry(&[(replacing_session_id, "S-quiet-lake")]),
        "the session stays in the list across the swap"
    );
}

/// A session record this router holds no exit watcher for, at `socket_address`.
fn build_unwatched_session_record(socket_address: String) -> SessionRecord {
    SessionRecord {
        session_name: "S-quiet-lake".to_string(),
        socket_address,
        process_id: 4242,
        has_exit_watcher: false,
    }
}

#[test]
fn the_liveness_probe_removes_only_unwatched_sessions_nothing_listens_for() {
    let gone_session_id = SessionId::new();
    let live_session_id = SessionId::new();
    let watched_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let gone_socket_address = compute_socket_address(runtime_directory.path(), gone_session_id);
    write_test_endpoint_file(
        runtime_directory.path(),
        gone_session_id,
        &gone_socket_address,
    );
    let live_socket_address = compute_socket_address(runtime_directory.path(), live_session_id);
    let live_session_listener = bind_test_session_listener(&live_socket_address);
    let mut session_registry = build_session_registry(&[(watched_session_id, "S-loud-river")]);
    session_registry.insert(
        gone_session_id,
        build_unwatched_session_record(gone_socket_address),
    );
    session_registry.insert(
        live_session_id,
        build_unwatched_session_record(live_socket_address.clone()),
    );
    let mut router_sessions = RouterSessions::from_session_registry(session_registry);
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    start_unwatched_session_probes(
        runtime_directory.path(),
        &mut router_sessions,
        &router_events_sender,
    );

    assert_eq!(
        router_sessions.probing_session_ids,
        BTreeSet::from([gone_session_id, live_session_id]),
        "the watched session is not probed"
    );
    settle_router_sessions(
        runtime_directory.path(),
        &mut router_sessions,
        &mut build_no_remote_state(),
        &router_events_sender,
        &router_events_receiver,
        is_nothing_in_flight,
    );
    let mut expected_session_registry =
        build_session_registry(&[(watched_session_id, "S-loud-river")]);
    expected_session_registry.insert(
        live_session_id,
        build_unwatched_session_record(live_socket_address),
    );
    assert_eq!(router_sessions.session_registry, expected_session_registry);
    assert!(
        !EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), gone_session_id)
            .exists(),
        "the gone session's endpoint file is removed"
    );
    drop(live_session_listener);
}

#[test]
fn an_unwatched_session_that_is_gone_leaves_the_list_and_the_loop_ends_idle() {
    let gone_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();

    let (router_exit, remaining_session_registry) = run_queued_dispatch_loop(
        runtime_directory.path(),
        SessionRegistry::from([(
            gone_session_id,
            build_unwatched_session_record(compute_socket_address(
                runtime_directory.path(),
                gone_session_id,
            )),
        )]),
        Vec::new(),
        TEST_LIVENESS_CHECK_INTERVAL_DURATION,
    );

    assert_eq!(router_exit, RouterExit::Idle);
    assert_eq!(remaining_session_registry, SessionRegistry::new());
}

#[test]
fn a_full_event_queue_does_not_hold_off_a_probe_that_is_due() {
    // The probe interval is zero, so a probe is due on the first turn. 20 000
    // row requests are queued before the loop starts. The probe reaches the
    // session's listener before the last row request is answered.
    const QUEUED_ROW_REQUEST_COUNT: usize = 20_000;
    let probed_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), probed_session_id);
    let session_listener = bind_test_session_listener(&socket_address);
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let rows_receivers: Vec<Receiver<Vec<RemoteSessionRow>>> = (0..QUEUED_ROW_REQUEST_COUNT)
        .map(|_| {
            let (rows_sender, rows_receiver) = mpsc::channel();
            router_events_sender
                .send(RouterEvent::Admission(AdmissionAsk::ListRows {
                    scope: TokenScope::HostWide,
                    response_sender: rows_sender,
                }))
                .expect("the row request is queued");
            rows_receiver
        })
        .collect();
    router_events_sender
        .send(RouterEvent::RestartDue)
        .expect("the restart is queued");
    let held_runtime_directory = runtime_directory.path().to_path_buf();
    let loop_thread = std::thread::spawn(move || {
        let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::from([(
            probed_session_id,
            build_unwatched_session_record(socket_address),
        )]));
        run_dispatch_loop(
            &held_runtime_directory,
            None,
            &build_test_executable_watch(),
            None,
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            Duration::ZERO,
            &mut router_sessions,
            &mut build_no_remote_state(),
        )
    });

    let (probe_accepted_sender, probe_accepted_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let probe_connection = session_listener.accept();
        let _ = probe_accepted_sender.send(());
        drop(probe_connection);
    });

    probe_accepted_receiver
        .recv_timeout(LOOP_END_TIMEOUT_DURATION)
        .expect("the probe reaches the session");
    assert_eq!(
        rows_receivers
            .last()
            .expect("row requests are queued")
            .try_recv(),
        Err(mpsc::TryRecvError::Empty),
        "the probe started before the queued requests were all served"
    );
    assert_eq!(join_dispatch_loop_thread(loop_thread), RouterExit::Restart);
}

#[test]
fn a_probe_reports_on_the_events_channel_and_a_second_start_while_it_runs_starts_nothing() {
    // Nothing listens at the listed address. The list changes only once the
    // probe's report is applied.
    let probed_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        probed_session_id,
        "S-quiet-lake",
    )]));
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    start_session_probe(
        runtime_directory.path(),
        &mut router_sessions,
        probed_session_id,
        &router_events_sender,
    )
    .expect("the probe starts");
    start_session_probe(
        runtime_directory.path(),
        &mut router_sessions,
        probed_session_id,
        &router_events_sender,
    )
    .expect("a second start while the probe runs is no failure");

    assert_eq!(
        router_sessions.probing_session_ids,
        BTreeSet::from([probed_session_id])
    );
    assert_eq!(
        router_sessions.session_registry,
        build_session_registry(&[(probed_session_id, "S-quiet-lake")]),
        "nothing changes before the probe's report is applied"
    );
    let probe_event = router_events_receiver
        .recv_timeout(LOOP_END_TIMEOUT_DURATION)
        .expect("the probe reports");
    assert_eq!(
        router_events_receiver
            .recv_timeout(NO_FURTHER_EVENT_DURATION)
            .err(),
        Some(RecvTimeoutError::Timeout),
        "one probe ran"
    );
    match probe_event {
        RouterEvent::SessionProbed {
            session_id,
            probed_endpoint_file,
            session_probe_outcome: SessionProbeOutcome::NoListener,
        } => {
            assert_eq!(session_id, probed_session_id);
            assert_eq!(probed_endpoint_file, None);
        }
        _ => panic!("the probe reported something other than finding nothing listening"),
    }
}

#[cfg(unix)]
#[test]
fn a_probe_starts_without_waiting_for_the_endpoint_file_it_reads() {
    // The endpoint file is a FIFO: opening it waits until a writer opens it.
    // The probe's thread waits there, and the start returns at once. The test
    // then writes `x`, which the probe reads as an unreadable endpoint file.
    let probed_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), probed_session_id);
    let endpoint_path_text = std::ffi::CString::new(endpoint_path.as_os_str().as_encoded_bytes())
        .expect("the path holds no NUL byte");
    // SAFETY: `endpoint_path_text` is a NUL-terminated path that lives for the
    // call.
    assert_eq!(
        unsafe { libc::mkfifo(endpoint_path_text.as_ptr(), 0o600) },
        0,
        "the FIFO is made"
    );
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        probed_session_id,
        "S-quiet-lake",
    )]));
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    let (probe_start_sender, probe_start_receiver) = mpsc::channel();
    let probe_runtime_directory = runtime_directory.path().to_path_buf();
    let probe_events_sender = router_events_sender.clone();
    std::thread::spawn(move || {
        let probe_start = start_session_probe(
            &probe_runtime_directory,
            &mut router_sessions,
            probed_session_id,
            &probe_events_sender,
        );
        let _ = probe_start_sender.send((probe_start.is_ok(), router_sessions.probing_session_ids));
    });

    assert_eq!(
        probe_start_receiver
            .recv_timeout(PROMPT_ANSWER_DURATION)
            .expect("the probe does not wait on the calling thread"),
        (true, BTreeSet::from([probed_session_id]))
    );
    std::fs::write(&endpoint_path, b"x").expect("the FIFO takes a writer");
    match router_events_receiver.recv_timeout(LOOP_END_TIMEOUT_DURATION) {
        Ok(RouterEvent::SessionProbed {
            session_id,
            probed_endpoint_file,
            session_probe_outcome:
                SessionProbeOutcome::EndpointFileUnreadable {
                    endpoint_file_error,
                },
        }) => {
            assert_eq!(session_id, probed_session_id);
            assert_eq!(probed_endpoint_file, None);
            assert_eq!(
                endpoint_file_error.to_string(),
                format!(
                    "endpoint file {} is unreadable: expected value at line 1 column 1",
                    endpoint_path.display()
                )
            );
        }
        Ok(_) => panic!("the probe reported something other than the unreadable file"),
        Err(receive_error) => panic!("the probe did not report: {receive_error:?}"),
    }
}

#[test]
fn a_probe_that_finds_nothing_while_the_endpoint_file_turned_unreadable_removes_nothing() {
    // The probe read a good endpoint file and found nothing listening. The
    // file cannot be read when the report is applied: it reads as rebound.
    let unreadable_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let probed_endpoint_file = EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), unreadable_session_id),
        connection_token: ConnectionToken::from_secret("a".repeat(64)),
        process_id: NO_SUCH_PROCESS_ID,
    };
    let endpoint_path =
        write_unreadable_endpoint_file(runtime_directory.path(), unreadable_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        unreadable_session_id,
        "S-quiet-lake",
    )]));

    let session_probe_verdict = apply_session_probe(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        unreadable_session_id,
        Some(&probed_endpoint_file),
        SessionProbeOutcome::NoListener,
    );

    assert_eq!(
        session_probe_verdict,
        SessionProbeVerdict::NothingListening(SessionRemoval::Rebound)
    );
    assert_eq!(
        router_sessions.session_registry,
        build_session_registry(&[(unreadable_session_id, "S-quiet-lake")])
    );
    assert!(endpoint_path.exists(), "the endpoint file is left in place");
}

#[test]
fn a_gone_answer_while_the_endpoint_file_turned_unreadable_removes_nothing() {
    // The description read a good endpoint file and found nothing listening.
    // The file cannot be read when the answer is applied.
    let unreadable_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let described_endpoint_file = EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), unreadable_session_id),
        connection_token: ConnectionToken::from_secret("a".repeat(64)),
        process_id: NO_SUCH_PROCESS_ID,
    };
    let endpoint_path =
        write_unreadable_endpoint_file(runtime_directory.path(), unreadable_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: unreadable_session_id,
            described_session_origin: DescribedSessionOrigin::ThisUser,
            described_endpoint_file: Some(described_endpoint_file),
            description_answer: Err(CliError::SessionNotFound {
                session_name: unreadable_session_id.to_string(),
            }),
        },
    );

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, unreadable_session_id),
        Some(format!(
            "endpoint file {} is unreadable: format 3 is not the 2 this build reads",
            endpoint_path.display()
        ))
    );
    assert!(endpoint_path.exists(), "the endpoint file is left in place");
}

#[test]
fn a_probe_that_finds_nothing_after_the_endpoint_file_was_rewritten_removes_nothing() {
    // The probe read one endpoint file and found nothing at its address. The
    // session has since bound again and written a new endpoint file.
    let rebound_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), rebound_session_id);
    write_test_endpoint_file(
        runtime_directory.path(),
        rebound_session_id,
        &socket_address,
    );
    let probed_endpoint_file = EndpointFile {
        socket_address,
        connection_token: ConnectionToken::from_secret("a".repeat(64)),
        process_id: NO_SUCH_PROCESS_ID,
    };
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        rebound_session_id,
        "S-quiet-lake",
    )]));
    router_sessions
        .probing_session_ids
        .insert(rebound_session_id);

    let session_probe_verdict = apply_session_probe(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        rebound_session_id,
        Some(&probed_endpoint_file),
        SessionProbeOutcome::NoListener,
    );

    assert_eq!(
        session_probe_verdict,
        SessionProbeVerdict::NothingListening(SessionRemoval::Rebound)
    );
    assert_eq!(
        router_sessions.session_registry,
        build_session_registry(&[(rebound_session_id, "S-quiet-lake")])
    );
    assert_eq!(router_sessions.probing_session_ids, BTreeSet::new());
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), rebound_session_id)
            .exists(),
        "the endpoint file is left in place"
    );
}

#[test]
fn a_probe_report_for_a_session_no_longer_listed_changes_nothing() {
    let removed_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file(
        runtime_directory.path(),
        removed_session_id,
        &compute_socket_address(runtime_directory.path(), removed_session_id),
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    router_sessions
        .probing_session_ids
        .insert(removed_session_id);

    let session_probe_verdict = apply_session_probe(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        removed_session_id,
        None,
        SessionProbeOutcome::NoListener,
    );

    assert_eq!(session_probe_verdict, SessionProbeVerdict::NotListed);
    assert_eq!(router_sessions.probing_session_ids, BTreeSet::new());
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), removed_session_id)
            .exists(),
        "the endpoint file is left in place"
    );
}

/// An attach lookup for `session_selector` waiting on the probe of
/// `probed_session_id`, awaiting no description, answering on
/// `response_sender`.
fn build_attach_lookup_waiting_on_probe(
    session_selector: SessionSelector,
    probed_session_id: SessionId,
    response_sender: Sender<RouterResult>,
) -> WaitingAttachLookup {
    WaitingAttachLookup {
        session_selector,
        awaited_session_ids: BTreeSet::new(),
        unlisted_session_count: 0,
        unread_paths: Vec::new(),
        answer_deadline: Instant::now() + SESSION_DISCOVERY_TIMEOUT_DURATION,
        probed_session_id: Some(probed_session_id),
        response_sender,
    }
}

#[test]
fn a_lookup_waiting_on_a_probe_that_found_the_session_rebound_waits_for_a_new_probe() {
    // The new probe finds nothing listening and no endpoint file: the session
    // is removed and the lookup answered.
    let rebound_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        rebound_session_id,
        "S-quiet-lake",
    )]));
    let (response_sender, response_receiver) = mpsc::channel();
    router_sessions
        .waiting_attach_lookups
        .push(build_attach_lookup_waiting_on_probe(
            SessionSelector::SessionId(rebound_session_id),
            rebound_session_id,
            response_sender,
        ));
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    hand_probe_verdict_to_attach_lookups(
        runtime_directory.path(),
        &mut router_sessions,
        &router_events_sender,
        rebound_session_id,
        &SessionProbeVerdict::NothingListening(SessionRemoval::Rebound),
    );

    assert_eq!(
        router_sessions.probing_session_ids,
        BTreeSet::from([rebound_session_id])
    );
    assert_eq!(router_sessions.waiting_attach_lookups.len(), 1);
    assert_eq!(response_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    settle_router_sessions(
        runtime_directory.path(),
        &mut router_sessions,
        &mut build_no_remote_state(),
        &router_events_sender,
        &router_events_receiver,
        is_nothing_in_flight,
    );
    assert_eq!(
        response_receiver.try_recv(),
        Ok(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: format!("no session {rebound_session_id} is running"),
        }))
    );
    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
}

#[test]
fn a_lookup_waiting_on_a_probe_of_a_session_since_removed_resolves_its_selector_again() {
    let removed_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (response_sender, response_receiver) = mpsc::channel();
    router_sessions
        .waiting_attach_lookups
        .push(build_attach_lookup_waiting_on_probe(
            SessionSelector::SessionId(removed_session_id),
            removed_session_id,
            response_sender,
        ));
    let router_events_sender = build_unread_router_events_sender();

    hand_probe_verdict_to_attach_lookups(
        runtime_directory.path(),
        &mut router_sessions,
        &router_events_sender,
        removed_session_id,
        &SessionProbeVerdict::NotListed,
    );

    assert_eq!(
        router_sessions
            .waiting_attach_lookups
            .iter()
            .map(|attach_lookup| attach_lookup.probed_session_id)
            .collect::<Vec<Option<SessionId>>>(),
        vec![None]
    );
    assert_eq!(response_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    advance_waiting_requests(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &build_no_remote_state(),
        &router_events_sender,
        Instant::now(),
    );
    assert_eq!(
        response_receiver.try_recv(),
        Ok(RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: format!("no session {removed_session_id} is running"),
        }))
    );
}

#[test]
fn only_the_lookups_waiting_on_a_probe_that_could_not_connect_are_refused_naming_the_failure() {
    // Two lookups wait on the probe of one session, and a third on another
    // session's probe.
    let unreachable_session_id = SessionId::new();
    let other_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[
        (unreachable_session_id, "S-quiet-lake"),
        (other_session_id, "S-loud-river"),
    ]));
    let (by_id_sender, by_id_receiver) = mpsc::channel();
    let (by_name_sender, by_name_receiver) = mpsc::channel();
    let (other_sender, other_receiver) = mpsc::channel();
    router_sessions.waiting_attach_lookups = vec![
        build_attach_lookup_waiting_on_probe(
            SessionSelector::SessionId(unreachable_session_id),
            unreachable_session_id,
            by_id_sender,
        ),
        build_attach_lookup_waiting_on_probe(
            SessionSelector::SessionName("S-quiet-lake".to_string()),
            unreachable_session_id,
            by_name_sender,
        ),
        build_attach_lookup_waiting_on_probe(
            SessionSelector::SessionId(other_session_id),
            other_session_id,
            other_sender,
        ),
    ];

    hand_probe_verdict_to_attach_lookups(
        runtime_directory.path(),
        &mut router_sessions,
        &build_unread_router_events_sender(),
        unreachable_session_id,
        &SessionProbeVerdict::Unreachable {
            connect_error_text: "all pipe instances are busy".to_string(),
        },
    );

    let expected_refusal =
        build_expected_refusal("the session could not be reached: all pipe instances are busy");
    assert_eq!(by_id_receiver.try_recv(), Ok(expected_refusal.clone()));
    assert_eq!(by_name_receiver.try_recv(), Ok(expected_refusal));
    assert_eq!(other_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(
        router_sessions
            .waiting_attach_lookups
            .iter()
            .map(|attach_lookup| attach_lookup.probed_session_id)
            .collect::<Vec<Option<SessionId>>>(),
        vec![Some(other_session_id)]
    );
    assert_eq!(
        router_sessions.session_registry,
        build_session_registry(&[
            (unreachable_session_id, "S-quiet-lake"),
            (other_session_id, "S-loud-river"),
        ]),
        "a session something is at stays listed"
    );
}

#[test]
fn a_sent_answer_keeps_its_wait_open_past_the_deadline_and_wakes_the_dispatcher_now() {
    let answered_session_id = SessionId::new();
    let now = Instant::now();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    insert_description_in_flight(
        &mut router_sessions,
        answered_session_id,
        compute_expired_asked_at(),
    );
    router_sessions.description_in_flight_by_session_id[&answered_session_id]
        .is_answer_sent
        .store(true, Ordering::SeqCst);
    let (response_sender, _response_receiver) = mpsc::channel();
    router_sessions
        .waiting_attach_lookups
        .push(WaitingAttachLookup {
            session_selector: SessionSelector::SessionId(answered_session_id),
            awaited_session_ids: BTreeSet::from([answered_session_id]),
            unlisted_session_count: 0,
            unread_paths: Vec::new(),
            answer_deadline: now,
            probed_session_id: None,
            response_sender,
        });

    assert_eq!(
        find_description_wait_end(
            &router_sessions,
            &BTreeSet::from([answered_session_id]),
            now,
            now
        ),
        Some(now)
    );
    assert_eq!(
        find_earliest_description_due_at(&router_sessions, now),
        Some(now)
    );
    assert_eq!(
        find_next_waiting_request_wake_at(&router_sessions, now),
        Some(now)
    );
}

#[test]
fn a_description_answer_sent_before_the_wait_ended_is_applied_before_the_lookup_is_answered() {
    // The answer sits in the queue when the description's 5 seconds run out.
    let described_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener =
        advertise_listening_session(runtime_directory.path(), described_session_id);
    let described_endpoint_file = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), described_session_id),
    )
    .expect("the endpoint file is read");
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    insert_description_in_flight(
        &mut router_sessions,
        described_session_id,
        compute_expired_asked_at(),
    );
    router_sessions.description_in_flight_by_session_id[&described_session_id]
        .is_answer_sent
        .store(true, Ordering::SeqCst);
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    router_events_sender
        .send(RouterEvent::SessionDescribed {
            session_id: described_session_id,
            described_session_origin: DescribedSessionOrigin::ThisUser,
            described_endpoint_file: Some(described_endpoint_file.clone()),
            description_answer: Ok(build_test_session_overview(
                described_session_id,
                "S-quiet-lake",
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )),
        })
        .expect("the answer is queued");
    let (response_sender, response_receiver) = mpsc::channel();
    router_sessions
        .waiting_attach_lookups
        .push(WaitingAttachLookup {
            session_selector: SessionSelector::SessionId(described_session_id),
            awaited_session_ids: BTreeSet::from([described_session_id]),
            unlisted_session_count: 0,
            unread_paths: Vec::new(),
            answer_deadline: Instant::now(),
            probed_session_id: None,
            response_sender,
        });

    advance_waiting_requests(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &build_no_remote_state(),
        &router_events_sender,
        Instant::now(),
    );

    assert_eq!(response_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    settle_router_sessions(
        runtime_directory.path(),
        &mut router_sessions,
        &mut build_no_remote_state(),
        &router_events_sender,
        &router_events_receiver,
        |router_sessions| router_sessions.waiting_attach_lookups.is_empty(),
    );
    assert_eq!(
        response_receiver.try_recv(),
        Ok(RouterResult::Found(SessionAddress {
            session_id: described_session_id,
            session_name: "S-quiet-lake".to_string(),
            socket_address: described_endpoint_file.socket_address,
            process_id: NO_SUCH_PROCESS_ID,
        }))
    );
    drop(session_listener);
}

#[test]
fn the_description_wait_ends_at_the_first_answer_due_or_the_deadline() {
    let first_session_id = SessionId::new();
    let second_session_id = SessionId::new();
    let unawaited_session_id = SessionId::new();
    let now = Instant::now();
    let first_asked_at = now
        .checked_sub(Duration::from_secs(1))
        .expect("this clock reaches back 1 second");
    let second_asked_at = now
        .checked_sub(Duration::from_secs(3))
        .expect("this clock reaches back 3 seconds");
    let first_due_at = first_asked_at + SESSION_DISCOVERY_TIMEOUT_DURATION;
    let second_due_at = second_asked_at + SESSION_DISCOVERY_TIMEOUT_DURATION;
    let answer_deadline = now + SESSION_DISCOVERY_TIMEOUT_DURATION;
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    insert_description_in_flight(&mut router_sessions, first_session_id, first_asked_at);
    insert_description_in_flight(&mut router_sessions, second_session_id, second_asked_at);
    insert_description_in_flight(&mut router_sessions, unawaited_session_id, now);
    let both_awaited = BTreeSet::from([first_session_id, second_session_id]);

    assert_eq!(
        find_description_wait_end(
            &router_sessions,
            &BTreeSet::from([first_session_id]),
            answer_deadline,
            now
        ),
        Some(first_due_at),
        "one awaited description is waited for until it stops being due"
    );
    assert_eq!(
        find_description_wait_end(&router_sessions, &both_awaited, answer_deadline, now),
        Some(second_due_at),
        "several are waited for until the first stops being due"
    );
    assert_eq!(
        find_description_wait_end(
            &router_sessions,
            &both_awaited,
            now + Duration::from_secs(1),
            now
        ),
        Some(now + Duration::from_secs(1)),
        "a deadline before every due time ends the wait at the deadline"
    );
    assert_eq!(
        find_description_wait_end(&router_sessions, &BTreeSet::new(), answer_deadline, now),
        None,
        "a request awaiting nothing waits for nothing"
    );
    assert_eq!(
        find_description_wait_end(
            &router_sessions,
            &both_awaited,
            answer_deadline,
            second_due_at
        ),
        Some(first_due_at),
        "a description asked 5 seconds ago is waited for no more"
    );
    assert_eq!(
        find_description_wait_end(
            &router_sessions,
            &both_awaited,
            answer_deadline,
            first_due_at
        ),
        None
    );
    assert_eq!(
        find_description_wait_end(
            &router_sessions,
            &both_awaited,
            answer_deadline,
            answer_deadline
        ),
        None,
        "nothing is waited for at the deadline"
    );

    router_sessions
        .description_in_flight_by_session_id
        .remove(&second_session_id);

    assert_eq!(
        find_description_wait_end(&router_sessions, &both_awaited, answer_deadline, now),
        Some(first_due_at),
        "a description that answered is waited for no more"
    );
}

#[test]
fn a_lookup_by_id_awaits_that_session_alone_and_a_lookup_by_name_awaits_every_unlisted_one() {
    // Neither endpoint file can be read: the scan records both and starts no
    // thread.
    let first_session_id = SessionId::new();
    let second_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_unreadable_endpoint_file(runtime_directory.path(), first_session_id);
    write_unreadable_endpoint_file(runtime_directory.path(), second_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let session_selectors = [
        SessionSelector::SessionId(first_session_id),
        SessionSelector::SessionId(SessionId::new()),
        SessionSelector::SessionName("S-quiet-lake".to_string()),
    ];

    for session_selector in session_selectors {
        start_attach_lookup(
            runtime_directory.path(),
            &mut router_sessions,
            &build_unread_router_events_sender(),
            ForeignSessionListing::default(),
            session_selector,
            mpsc::channel().0,
        );
    }

    let awaited_session_ids: Vec<BTreeSet<SessionId>> = router_sessions
        .waiting_attach_lookups
        .iter()
        .map(|attach_lookup| attach_lookup.awaited_session_ids.clone())
        .collect();
    assert_eq!(
        awaited_session_ids,
        vec![
            BTreeSet::from([first_session_id]),
            BTreeSet::new(),
            BTreeSet::from([first_session_id, second_session_id]),
        ]
    );
}

#[test]
fn a_reported_exit_for_a_session_that_still_listens_keeps_it_listed_and_unwatched() {
    // A Windows session that replaced its image serves on from a new process
    // at the same address. The exit of the old process is reported.
    let swapped_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener =
        advertise_listening_session(runtime_directory.path(), swapped_session_id);
    let socket_address = compute_socket_address(runtime_directory.path(), swapped_session_id);
    let mut session_registry = build_session_registry(&[(swapped_session_id, "S-quiet-lake")]);
    session_registry
        .get_mut(&swapped_session_id)
        .expect("the session is listed")
        .socket_address
        .clone_from(&socket_address);
    let mut router_sessions = RouterSessions::from_session_registry(session_registry);

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::ChildExited(swapped_session_id),
    );

    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            swapped_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: NO_SUCH_PROCESS_ID,
                has_exit_watcher: false,
            },
        )]),
        "the record takes the process id the endpoint file names"
    );
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), swapped_session_id)
            .exists(),
        "the endpoint file of the image that serves is left in place"
    );
    drop(session_listener);
}

/// List `session_id` as `S-quiet-lake` at an address nothing listens at,
/// while its endpoint file in `runtime_directory` names the address a bound
/// listener serves. Hands back the list, that served address, and the
/// listener.
///
/// This is a session that restarted at a new address: on Unix, turning
/// `allow-other-users` on moves its socket into the shared directory.
fn build_session_registry_listing_a_moved_session(
    runtime_directory: &Path,
    session_id: SessionId,
) -> (SessionRegistry, String, Listener) {
    let session_listener = advertise_listening_session(runtime_directory, session_id);
    let served_socket_address = compute_socket_address(runtime_directory, session_id);
    let mut session_registry = build_session_registry(&[(session_id, "S-quiet-lake")]);
    session_registry
        .get_mut(&session_id)
        .expect("the session is listed")
        .socket_address = compute_socket_address(runtime_directory, SessionId::new());
    (session_registry, served_socket_address, session_listener)
}

#[test]
fn a_reported_exit_probes_the_address_the_endpoint_file_names_now() {
    let moved_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (session_registry, served_socket_address, session_listener) =
        build_session_registry_listing_a_moved_session(runtime_directory.path(), moved_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(session_registry);

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::ChildExited(moved_session_id),
    );

    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            moved_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address: served_socket_address,
                process_id: NO_SUCH_PROCESS_ID,
                has_exit_watcher: false,
            },
        )])
    );
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), moved_session_id)
            .exists(),
        "the endpoint file of the session that serves is left in place"
    );
    drop(session_listener);
}

#[test]
fn a_lookup_hands_out_the_address_the_endpoint_file_names_now() {
    let moved_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (session_registry, served_socket_address, session_listener) =
        build_session_registry_listing_a_moved_session(runtime_directory.path(), moved_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(session_registry);

    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionId(moved_session_id),
    );

    assert_eq!(
        router_result,
        RouterResult::Found(SessionAddress {
            session_id: moved_session_id,
            session_name: "S-quiet-lake".to_string(),
            socket_address: served_socket_address.clone(),
            process_id: NO_SUCH_PROCESS_ID,
        })
    );
    assert_eq!(
        router_sessions.session_registry[&moved_session_id].socket_address,
        served_socket_address
    );
    drop(session_listener);
}

#[test]
fn a_reported_exit_for_a_session_whose_endpoint_file_cannot_be_read_removes_nothing() {
    // Nothing listens at the listed address, and the endpoint file, which
    // would name where the session listens now, cannot be read.
    let unreadable_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        write_unreadable_endpoint_file(runtime_directory.path(), unreadable_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        unreadable_session_id,
        "S-quiet-lake",
    )]));

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::ChildExited(unreadable_session_id),
    );

    let mut expected_session_registry =
        build_session_registry(&[(unreadable_session_id, "S-quiet-lake")]);
    expected_session_registry
        .get_mut(&unreadable_session_id)
        .expect("the session is listed")
        .has_exit_watcher = false;
    assert_eq!(router_sessions.session_registry, expected_session_registry);
    assert!(endpoint_path.exists(), "the endpoint file is left in place");
}

#[test]
fn a_lookup_for_a_listed_session_whose_endpoint_file_cannot_be_read_names_the_failure() {
    let unreadable_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let endpoint_path =
        write_unreadable_endpoint_file(runtime_directory.path(), unreadable_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        unreadable_session_id,
        "S-quiet-lake",
    )]));

    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionId(unreadable_session_id),
    );

    assert_eq!(
        router_result,
        build_expected_refusal(&format!(
            "session {unreadable_session_id} is running but did not answer: endpoint file {} is \
             unreadable: format 3 is not the 2 this build reads",
            endpoint_path.display()
        ))
    );
    assert_eq!(
        router_sessions.session_registry,
        build_session_registry(&[(unreadable_session_id, "S-quiet-lake")]),
        "the session stays listed as it was"
    );
    assert!(endpoint_path.exists(), "the endpoint file is left in place");
}

#[test]
fn a_reported_exit_while_a_session_replaces_its_image_keeps_it_listed_and_unwatched() {
    // Nothing listens at the address, and the resume file is inside the
    // window: the session stays, and the liveness probe takes it over.
    let replacing_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    build_aged_resume_file(
        runtime_directory.path(),
        replacing_session_id,
        INSIDE_RESTART_WINDOW_DURATION,
    );
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        replacing_session_id,
        "S-quiet-lake",
    )]));

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::ChildExited(replacing_session_id),
    );

    let mut expected_session_registry =
        build_session_registry(&[(replacing_session_id, "S-quiet-lake")]);
    expected_session_registry
        .get_mut(&replacing_session_id)
        .expect("the session is listed")
        .has_exit_watcher = false;
    assert_eq!(router_sessions.session_registry, expected_session_registry);
}

#[test]
fn a_reported_exit_removes_a_session_nothing_listens_for_at_once() {
    // The probe interval is one hour: only the exit report itself can empty
    // the list before the 5 second wait below runs out.
    const NEVER_DUE_LIVENESS_CHECK_INTERVAL_DURATION: Duration = Duration::from_secs(3600);
    let exited_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();

    let (router_exit, remaining_session_registry) = run_queued_dispatch_loop(
        runtime_directory.path(),
        build_session_registry(&[(exited_session_id, "S-quiet-lake")]),
        vec![RouterEvent::ChildExited(exited_session_id)],
        NEVER_DUE_LIVENESS_CHECK_INTERVAL_DURATION,
    );

    assert_eq!(router_exit, RouterExit::Idle);
    assert_eq!(remaining_session_registry, SessionRegistry::new());
}

#[test]
fn a_reported_exit_for_a_session_the_list_does_not_hold_removes_no_file() {
    // The endpoint file names a session the list does not hold: the report
    // starts no probe and leaves the file on the disk.
    let unlisted_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file(
        runtime_directory.path(),
        unlisted_session_id,
        &compute_socket_address(runtime_directory.path(), unlisted_session_id),
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::ChildExited(unlisted_session_id),
    );

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), unlisted_session_id)
            .exists(),
        "the endpoint file is left in place"
    );
}

#[test]
fn a_remote_row_request_answers_at_once_and_lists_a_session_once_its_answer_arrives() {
    // The stand-in takes the description and answers it only once released.
    // The first rows are answered without it. Its answer then reaches the
    // dispatcher as `SessionDescribed`, and the next rows list it.
    let unlisted_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), unlisted_session_id);
    let (release_sender, release_receiver) = mpsc::channel();
    let session_server_thread = spawn_session_server_answering_once_released(
        &socket_address,
        unlisted_session_id,
        "S-quiet-lake",
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        release_receiver,
    );
    write_test_endpoint_file(
        runtime_directory.path(),
        unlisted_session_id,
        &socket_address,
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let list_session_rows = |router_sessions: &mut RouterSessions| {
        let (rows_sender, rows_receiver) = mpsc::channel();
        serve_remote_admission(
            runtime_directory.path(),
            None,
            router_sessions,
            &mut build_no_remote_state(),
            &router_events_sender,
            AdmissionAsk::ListRows {
                scope: TokenScope::HostWide,
                response_sender: rows_sender,
            },
        );
        rows_receiver.recv().expect("the rows are answered")
    };

    let first_rows_started_at = Instant::now();
    assert_eq!(list_session_rows(&mut router_sessions), Vec::new());
    assert!(
        first_rows_started_at.elapsed() < PROMPT_ANSWER_DURATION,
        "the rows do not wait for the unanswered description"
    );
    drop(release_sender);
    let described_event = router_events_receiver
        .recv_timeout(LOOP_END_TIMEOUT_DURATION)
        .expect("the answer is reported");
    serve_router_event(
        runtime_directory.path(),
        None,
        &build_test_executable_watch(),
        None,
        &mut router_sessions,
        &mut build_no_remote_state(),
        &router_events_sender,
        described_event,
    );
    assert_eq!(
        list_session_rows(&mut router_sessions),
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
fn a_late_answer_that_reaches_the_dispatcher_registers_the_session() {
    // The session was asked 5 seconds ago. Its answer arrives after every
    // lookup waiting for it was answered.
    let described_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let socket_address = compute_socket_address(runtime_directory.path(), described_session_id);
    write_test_endpoint_file(
        runtime_directory.path(),
        described_session_id,
        &socket_address,
    );
    let described_endpoint_file = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), described_session_id),
    )
    .expect("the endpoint file reads back");
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    insert_description_in_flight(
        &mut router_sessions,
        described_session_id,
        compute_expired_asked_at(),
    );

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: described_session_id,
            described_session_origin: DescribedSessionOrigin::ThisUser,
            described_endpoint_file: Some(described_endpoint_file),
            description_answer: Ok(build_test_session_overview(
                described_session_id,
                "S-quiet-lake",
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )),
        },
    );

    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            described_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: NO_SUCH_PROCESS_ID,
                has_exit_watcher: false,
            },
        )])
    );
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::new()
    );
}

#[test]
fn an_answer_for_a_session_already_listed_changes_nothing() {
    let listed_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        listed_session_id,
        "S-quiet-lake",
    )]));
    insert_description_in_flight(&mut router_sessions, listed_session_id, Instant::now());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: listed_session_id,
            described_session_origin: DescribedSessionOrigin::ThisUser,
            described_endpoint_file: None,
            description_answer: Ok(build_test_session_overview(
                listed_session_id,
                "S-loud-river",
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )),
        },
    );

    assert_eq!(
        router_sessions.session_registry,
        build_session_registry(&[(listed_session_id, "S-quiet-lake")])
    );
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::new()
    );
}

/// The control-socket address of `session_id` another local user started,
/// under the placeholder shared directory `/srv/koshi-shared/5000`. Nothing
/// listens there.
fn build_foreign_socket_address(session_id: SessionId) -> String {
    compute_socket_address(Path::new("/srv/koshi-shared/5000"), session_id)
}

#[test]
fn a_lookup_by_id_asks_only_the_other_users_session_it_names() {
    let first_foreign_session_id = SessionId::new();
    let second_foreign_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let mut planted_listeners = Vec::new();
    for foreign_session_id in [first_foreign_session_id, second_foreign_session_id] {
        planted_listeners.push(bind_test_session_listener(&advertise_foreign_session(
            shared_sessions_base_directory.path(),
            runtime_directory.path(),
            foreign_session_id,
        )));
    }
    let mut by_id_router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let mut by_name_router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    for (router_sessions, session_selector) in [
        (
            &mut by_id_router_sessions,
            SessionSelector::SessionId(first_foreign_session_id),
        ),
        (
            &mut by_name_router_sessions,
            SessionSelector::SessionName("S-quiet-lake".to_string()),
        ),
    ] {
        let foreign_session_listing = list_foreign_sessions_for_lookup(
            Some(shared_sessions_base_directory.path()),
            runtime_directory.path(),
            &router_sessions.session_registry,
            &session_selector,
        )
        .expect("the shared directory is read");
        start_attach_lookup(
            runtime_directory.path(),
            router_sessions,
            &build_unread_router_events_sender(),
            foreign_session_listing,
            session_selector,
            mpsc::channel().0,
        );
    }

    assert_eq!(
        list_describing_session_ids(&by_id_router_sessions),
        BTreeSet::from([first_foreign_session_id])
    );
    assert_eq!(
        list_describing_session_ids(&by_name_router_sessions),
        BTreeSet::from([first_foreign_session_id, second_foreign_session_id])
    );
    assert!(by_name_router_sessions
        .description_in_flight_by_session_id
        .values()
        .all(|description_in_flight| description_in_flight.is_other_user_session));
}

#[test]
fn a_listing_that_could_not_read_a_path_asks_no_other_users_session() {
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let listed_foreign_session_id = SessionId::new();
    let duplicated_foreign_session_id = SessionId::new();
    let duplicated_session = DuplicatedForeignSession {
        session_id: duplicated_foreign_session_id,
        advertisement_count: 2,
        owner_user_ids: vec![1001, 1002],
    };

    let surveyed_session_ids = start_unlisted_session_descriptions(
        runtime_directory.path(),
        &mut router_sessions,
        &build_unread_router_events_sender(),
        Vec::new(),
        ForeignSessionListing {
            foreign_sessions: vec![(
                listed_foreign_session_id,
                build_foreign_socket_address(listed_foreign_session_id),
            )],
            duplicated_sessions: vec![duplicated_session],
            unlisted_session_count: 0,
            unread_path: Some(UnreadPath {
                looked_up_path: PathBuf::from("/srv/koshi-shared"),
                read_error_text: "it holds more than 256 user folders".to_string(),
            }),
        },
    );

    assert_eq!(
        surveyed_session_ids,
        BTreeSet::from([duplicated_foreign_session_id])
    );
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::new()
    );
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, listed_foreign_session_id),
        None
    );
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, duplicated_foreign_session_id),
        Some(format!(
            "session {duplicated_foreign_session_id} is advertised 2 times in the shared \
             directory, by user ids 1001, 1002; koshi reaches none of them"
        ))
    );
}

#[test]
fn an_other_users_session_found_past_the_description_limit_is_not_asked() {
    // 16 descriptions of other users' sessions already run, with no thread
    // behind them.
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let running_foreign_session_ids: Vec<SessionId> = (0..MAX_OTHER_USER_DESCRIPTIONS_IN_FLIGHT)
        .map(|_| SessionId::new())
        .collect();
    for running_foreign_session_id in &running_foreign_session_ids {
        insert_description_in_flight(
            &mut router_sessions,
            *running_foreign_session_id,
            Instant::now(),
        );
        router_sessions
            .description_in_flight_by_session_id
            .get_mut(running_foreign_session_id)
            .expect("the description was inserted")
            .is_other_user_session = true;
    }
    let unasked_foreign_session_id = SessionId::new();
    let running_foreign_session_id = running_foreign_session_ids[0];

    let surveyed_session_ids = start_unlisted_session_descriptions(
        runtime_directory.path(),
        &mut router_sessions,
        &build_unread_router_events_sender(),
        ipc_client::list_own_sessions(runtime_directory.path())
            .expect("read the runtime directory"),
        ForeignSessionListing {
            foreign_sessions: vec![
                (
                    unasked_foreign_session_id,
                    build_foreign_socket_address(unasked_foreign_session_id),
                ),
                (
                    running_foreign_session_id,
                    build_foreign_socket_address(running_foreign_session_id),
                ),
            ],
            ..ForeignSessionListing::default()
        },
    );

    assert_eq!(
        surveyed_session_ids,
        BTreeSet::from([unasked_foreign_session_id, running_foreign_session_id])
    );
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        running_foreign_session_ids
            .iter()
            .copied()
            .collect::<BTreeSet<SessionId>>()
    );
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, unasked_foreign_session_id),
        Some(
            "16 sessions other local users started are already being asked; run the command again"
                .to_string()
        )
    );
    assert!(
        !router_sessions
            .unanswered_reason_by_session_id
            .contains_key(&running_foreign_session_id),
        "a session already being asked records no reason"
    );
}

#[test]
fn a_late_answer_from_a_session_another_local_user_started_registers_it() {
    let foreign_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let foreign_socket_address = build_foreign_socket_address(foreign_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    insert_description_in_flight(
        &mut router_sessions,
        foreign_session_id,
        compute_expired_asked_at(),
    );

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: foreign_session_id,
            described_session_origin: DescribedSessionOrigin::OtherUser {
                socket_address: foreign_socket_address.clone(),
            },
            described_endpoint_file: None,
            description_answer: Ok(build_test_session_overview(
                foreign_session_id,
                "S-quiet-lake",
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )),
        },
    );

    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            foreign_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address: foreign_socket_address,
                process_id: 0,
                has_exit_watcher: false,
            },
        )])
    );
}

#[test]
fn an_answer_from_another_users_session_holding_an_id_this_user_advertises_registers_nothing() {
    // This user's endpoint file names the id: this user's session holds it.
    let shared_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file(
        runtime_directory.path(),
        shared_session_id,
        &compute_socket_address(runtime_directory.path(), shared_session_id),
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: shared_session_id,
            described_session_origin: DescribedSessionOrigin::OtherUser {
                socket_address: build_foreign_socket_address(shared_session_id),
            },
            described_endpoint_file: None,
            description_answer: Ok(build_test_session_overview(
                shared_session_id,
                "S-quiet-lake",
                UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            )),
        },
    );

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, shared_session_id),
        None
    );
}

#[test]
fn a_failed_description_of_another_users_session_records_the_failure() {
    let foreign_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: foreign_session_id,
            described_session_origin: DescribedSessionOrigin::OtherUser {
                socket_address: build_foreign_socket_address(foreign_session_id),
            },
            described_endpoint_file: None,
            description_answer: Err(CliError::IpcUnavailable {
                detail: "the session refused the empty token".to_string(),
            }),
        },
    );

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, foreign_session_id),
        Some("IPC unavailable: the session refused the empty token".to_string())
    );
}

#[test]
fn a_gone_answer_from_another_users_session_removes_nothing_of_that_user() {
    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_directory = build_test_runtime_directory();
    let foreign_session_id = SessionId::new();
    let foreign_socket_address = advertise_foreign_session(
        shared_sessions_directory.path(),
        runtime_directory.path(),
        foreign_session_id,
    );
    let leftover_advert_path = if cfg!(unix) {
        std::fs::write(&foreign_socket_address, b"").expect("plant the leftover socket file");
        PathBuf::from(&foreign_socket_address)
    } else {
        resolve_advertisement_marker_path(shared_sessions_directory.path(), foreign_session_id)
    };
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    serve_router_event_and_settle(
        runtime_directory.path(),
        &mut router_sessions,
        RouterEvent::SessionDescribed {
            session_id: foreign_session_id,
            described_session_origin: DescribedSessionOrigin::OtherUser {
                socket_address: foreign_socket_address,
            },
            described_endpoint_file: None,
            description_answer: Err(CliError::SessionNotFound {
                session_name: foreign_session_id.to_string(),
            }),
        },
    );

    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, foreign_session_id),
        None
    );
    assert!(
        leftover_advert_path.exists(),
        "the other user's advert is left alone"
    );
}

#[test]
fn a_lookup_finds_a_session_another_local_user_started_after_the_router_started() {
    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_directory = build_test_runtime_directory();
    let mut router_sessions = create_settled_router_sessions(
        runtime_directory.path(),
        Some(shared_sessions_directory.path()),
    );
    let foreign_session_id = SessionId::new();
    let foreign_socket_address = advertise_foreign_session(
        shared_sessions_directory.path(),
        runtime_directory.path(),
        foreign_session_id,
    );
    let session_server_thread = spawn_session_server_answering_one_connection(
        &foreign_socket_address,
        foreign_session_id,
        "S-quiet-lake",
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    );

    let router_result = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        Some(shared_sessions_directory.path()),
        SessionSelector::SessionName("S-quiet-lake".to_string()),
    );

    assert_eq!(
        router_result,
        RouterResult::Found(SessionAddress {
            session_id: foreign_session_id,
            session_name: "S-quiet-lake".to_string(),
            socket_address: foreign_socket_address,
            process_id: 0,
        })
    );
    drop(
        session_server_thread
            .join()
            .expect("the other user's session ended"),
    );
}

#[cfg(unix)]
#[test]
fn the_description_scan_skips_a_session_a_starting_session_server_holds() {
    // The session server bound and wrote its endpoint file, and has not
    // printed its ready line. It is not asked, and an answer that arrives for
    // it changes nothing: its ready line registers it.
    let starting_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file(
        runtime_directory.path(),
        starting_session_id,
        &compute_socket_address(runtime_directory.path(), starting_session_id),
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (response_sender, response_receiver) = mpsc::channel();
    router_sessions
        .starting_session_servers
        .push(StartingSessionServer {
            session_id: starting_session_id,
            session_name: "S-quiet-lake".to_string(),
            child_process: spawn_running_child("sleep 30"),
            ready_deadline: Instant::now() + SESSION_SERVER_READY_TIMEOUT_DURATION,
            is_ready_report_sent: Arc::new(AtomicBool::new(false)),
            response_sender,
        });

    let surveyed_session_ids = start_unlisted_session_descriptions(
        runtime_directory.path(),
        &mut router_sessions,
        &build_unread_router_events_sender(),
        ipc_client::list_own_sessions(runtime_directory.path())
            .expect("read the runtime directory"),
        ForeignSessionListing::default(),
    );
    apply_session_description(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &build_unread_router_events_sender(),
        starting_session_id,
        &DescribedSessionOrigin::ThisUser,
        None,
        Ok(build_test_session_overview(
            starting_session_id,
            "S-loud-river",
            UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )),
    );

    assert_eq!(surveyed_session_ids, BTreeSet::new());
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::new()
    );
    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(response_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    for mut starting_session_server in router_sessions.starting_session_servers {
        terminate_child_process(&mut starting_session_server.child_process);
    }
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
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let mut remote_state = build_remote_state_admitting_one_connection();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let (endpoint_sender, endpoint_receiver) = mpsc::channel();

    serve_remote_admission(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &mut remote_state,
        &router_events_sender,
        AdmissionAsk::Locate {
            scope: TokenScope::HostWide,
            remote_connection_id: 0,
            session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
            response_sender: endpoint_sender,
        },
    );
    settle_router_sessions(
        runtime_directory.path(),
        &mut router_sessions,
        &mut remote_state,
        &router_events_sender,
        &router_events_receiver,
        |router_sessions| router_sessions.waiting_remote_locates.is_empty(),
    );

    assert_eq!(
        endpoint_receiver.try_recv(),
        Ok(Ok(EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            unlisted_session_id
        )))
    );
    let listener = session_server_thread
        .join()
        .expect("the stand-in session ended");
    drop(listener);
}

/// Remote access holding one admitted connection, registered under number
/// `0`, from a loopback socket pair whose caller end is dropped.
fn build_remote_state_admitting_one_connection() -> RemoteState {
    let mut remote_state = build_no_remote_state();
    let (admitted_stream, _caller_stream) = build_loopback_connection_pair();
    remote_state
        .admitted_remote_connections
        .push(AdmittedRemoteConnection {
            token_hash: "g".repeat(64),
            tcp_stream: admitted_stream,
            remote_connection_id: 0,
        });
    remote_state
}

#[test]
fn a_remote_locate_request_for_a_silent_session_waits_without_holding_the_dispatcher() {
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener = advertise_listening_session(runtime_directory.path(), silent_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let mut remote_state = build_remote_state_admitting_one_connection();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let (endpoint_sender, endpoint_receiver) = mpsc::channel();

    let locate_started_at = Instant::now();
    serve_remote_admission(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &mut remote_state,
        &router_events_sender,
        AdmissionAsk::Locate {
            scope: TokenScope::HostWide,
            remote_connection_id: 0,
            session_selector: SessionSelector::SessionId(silent_session_id),
            response_sender: endpoint_sender,
        },
    );

    assert!(
        locate_started_at.elapsed() < PROMPT_ANSWER_DURATION,
        "the request joins the waiting list without waiting"
    );
    assert_eq!(endpoint_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(router_sessions.waiting_remote_locates.len(), 1);

    insert_description_in_flight(
        &mut router_sessions,
        silent_session_id,
        compute_expired_asked_at(),
    );
    settle_router_sessions(
        runtime_directory.path(),
        &mut router_sessions,
        &mut remote_state,
        &router_events_sender,
        &router_events_receiver,
        |router_sessions| router_sessions.waiting_remote_locates.is_empty(),
    );

    assert_eq!(
        endpoint_receiver.try_recv(),
        Ok(Err(LocateRefusal::NotReached))
    );
    drop(session_listener);
}

#[test]
fn a_remote_locate_request_for_a_listed_name_waits_for_every_unlisted_session_to_describe_itself() {
    // The name is listed, and a session of this user's is still being asked:
    // the request waits all the same.
    let listed_session_id = SessionId::new();
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let session_listener = advertise_listening_session(runtime_directory.path(), silent_session_id);
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        listed_session_id,
        "S-quiet-lake",
    )]));
    let mut remote_state = build_remote_state_admitting_one_connection();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let (endpoint_sender, endpoint_receiver) = mpsc::channel();

    serve_remote_admission(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &mut remote_state,
        &router_events_sender,
        AdmissionAsk::Locate {
            scope: TokenScope::HostWide,
            remote_connection_id: 0,
            session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
            response_sender: endpoint_sender,
        },
    );
    advance_waiting_requests(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &remote_state,
        &router_events_sender,
        Instant::now(),
    );

    assert_eq!(endpoint_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(router_sessions.waiting_remote_locates.len(), 1);

    insert_description_in_flight(
        &mut router_sessions,
        silent_session_id,
        compute_expired_asked_at(),
    );
    settle_router_sessions(
        runtime_directory.path(),
        &mut router_sessions,
        &mut remote_state,
        &router_events_sender,
        &router_events_receiver,
        |router_sessions| router_sessions.waiting_remote_locates.is_empty(),
    );

    assert_eq!(
        endpoint_receiver.try_recv(),
        Ok(Ok(EndpointFile::resolve_endpoint_file_path(
            runtime_directory.path(),
            listed_session_id
        )))
    );
    drop(session_listener);
}

#[test]
fn a_session_creation_waits_while_a_description_asked_less_than_5_seconds_ago_is_unanswered() {
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let asked_at = Instant::now();
    insert_description_in_flight(&mut router_sessions, SessionId::new(), asked_at);
    let (response_sender, response_receiver) = mpsc::channel();
    router_sessions
        .queued_session_creations
        .push(QueuedSessionCreation {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
            response_sender,
        });

    advance_session_creations(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &build_unread_router_events_sender(),
        asked_at,
    );

    assert_eq!(router_sessions.queued_session_creations.len(), 1);
    assert_eq!(router_sessions.starting_session_servers.len(), 0);
    assert_eq!(response_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(
        find_next_waiting_request_wake_at(&router_sessions, asked_at),
        Some(asked_at + SESSION_DISCOVERY_TIMEOUT_DURATION),
        "the dispatcher looks again once the description stops being due"
    );
}

#[test]
fn a_session_creation_does_not_wait_for_a_description_asked_5_seconds_ago() {
    // The silent session was asked 5 seconds ago and never answered. The
    // creation starts its session server: this test binary, which exits at
    // once.
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    insert_description_in_flight(
        &mut router_sessions,
        SessionId::new(),
        compute_expired_asked_at(),
    );
    let (response_sender, response_receiver) = mpsc::channel();
    router_sessions
        .queued_session_creations
        .push(QueuedSessionCreation {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
            response_sender,
        });
    let now = Instant::now();

    advance_session_creations(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &build_unread_router_events_sender(),
        now,
    );

    assert_eq!(router_sessions.queued_session_creations.len(), 0);
    assert_eq!(router_sessions.starting_session_servers.len(), 1);
    assert_eq!(response_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(
        find_next_waiting_request_wake_at(&router_sessions, now),
        Some(router_sessions.starting_session_servers[0].ready_deadline),
        "the dispatcher looks again at the ready deadline, not at the old description"
    );
    for mut starting_session_server in router_sessions.starting_session_servers {
        terminate_child_process(&mut starting_session_server.child_process);
    }
}

#[test]
fn a_session_server_that_prints_no_ready_line_is_refused_and_leaves_nothing_behind() {
    // The session server started is this test binary. It reads the session
    // server arguments as an option it does not know, and exits without
    // printing a ready line.
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (response_sender, response_receiver) = mpsc::channel();
    router_sessions
        .queued_session_creations
        .push(QueuedSessionCreation {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
            response_sender,
        });
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    settle_router_sessions(
        runtime_directory.path(),
        &mut router_sessions,
        &mut build_no_remote_state(),
        &router_events_sender,
        &router_events_receiver,
        is_nothing_in_flight,
    );

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_expected_refusal(
            "the session did not report a bound socket"
        ))
    );
    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(
        ipc_client::list_advertised_sessions(runtime_directory.path()),
        Ok(Vec::new())
    );
}

/// Whether the process `process_id` is gone and collected: a signal `0` sent
/// to it fails.
#[cfg(unix)]
fn is_process_collected(process_id: u32) -> bool {
    // SAFETY: signal `0` checks that the process exists and sends nothing.
    unsafe { libc::kill(process_id as libc::pid_t, 0) == -1 }
}

/// A session server this router started under `session_id` and
/// `S-quiet-lake`, running `sleep 30`, whose ready line has not arrived by
/// `ready_deadline`. Hands back the starting server and the receiver its
/// creation is answered on.
#[cfg(unix)]
fn build_starting_session_server(
    session_id: SessionId,
    ready_deadline: Instant,
) -> (StartingSessionServer, Receiver<RouterResult>) {
    let (response_sender, response_receiver) = mpsc::channel();
    (
        StartingSessionServer {
            session_id,
            session_name: "S-quiet-lake".to_string(),
            child_process: spawn_running_child("sleep 30"),
            ready_deadline,
            is_ready_report_sent: Arc::new(AtomicBool::new(false)),
            response_sender,
        },
        response_receiver,
    )
}

#[cfg(unix)]
#[test]
fn a_starting_session_server_past_its_ready_deadline_is_killed_and_refused() {
    // The session server bound and wrote its endpoint file, and never printed
    // its ready line.
    let starting_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (starting_session_server, response_receiver) =
        build_starting_session_server(starting_session_id, Instant::now());
    let child_process_id = starting_session_server.child_process.id();
    write_test_endpoint_file_for_process(
        runtime_directory.path(),
        starting_session_id,
        &compute_socket_address(runtime_directory.path(), starting_session_id),
        child_process_id,
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    router_sessions
        .starting_session_servers
        .push(starting_session_server);

    advance_session_creations(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &build_unread_router_events_sender(),
        Instant::now(),
    );

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_expected_refusal(
            "the session did not report a bound socket"
        ))
    );
    assert_eq!(router_sessions.starting_session_servers.len(), 0);
    assert!(is_process_collected(child_process_id));
    assert!(
        !EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), starting_session_id)
            .exists(),
        "the endpoint file the session server wrote is removed"
    );
}

#[cfg(unix)]
#[test]
fn a_ready_report_registers_the_session_and_answers_its_creation() {
    let starting_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (starting_session_server, response_receiver) = build_starting_session_server(
        starting_session_id,
        Instant::now() + SESSION_SERVER_READY_TIMEOUT_DURATION,
    );
    let child_process_id = starting_session_server.child_process.id();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    router_sessions
        .starting_session_servers
        .push(starting_session_server);
    let socket_address = compute_socket_address(runtime_directory.path(), starting_session_id);
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    finish_session_creation(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &router_events_sender,
        starting_session_id,
        Some(SessionServerReady {
            protocol_version: ROUTER_PROTOCOL_VERSION,
            socket_address: socket_address.clone(),
        }),
    );

    assert_eq!(
        response_receiver.try_recv(),
        Ok(RouterResult::Created(SessionAddress {
            session_id: starting_session_id,
            session_name: "S-quiet-lake".to_string(),
            socket_address: socket_address.clone(),
            process_id: child_process_id,
        }))
    );
    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            starting_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: child_process_id,
                has_exit_watcher: true,
            },
        )])
    );
    assert_eq!(router_sessions.starting_session_servers.len(), 0);
    // SAFETY: the signal goes to the child this test started.
    unsafe {
        libc::kill(child_process_id as libc::pid_t, libc::SIGKILL);
    }
    match router_events_receiver.recv_timeout(LOOP_END_TIMEOUT_DURATION) {
        Ok(RouterEvent::ChildExited(exited_session_id)) => {
            assert_eq!(exited_session_id, starting_session_id)
        }
        Ok(_) => panic!("the reaper reported something other than the exit"),
        Err(receive_error) => panic!("the exit was not reported: {receive_error:?}"),
    }
}

#[cfg(unix)]
#[test]
fn a_ready_report_from_another_build_kills_the_session_server_and_refuses_the_creation() {
    let starting_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (starting_session_server, response_receiver) = build_starting_session_server(
        starting_session_id,
        Instant::now() + SESSION_SERVER_READY_TIMEOUT_DURATION,
    );
    let child_process_id = starting_session_server.child_process.id();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    router_sessions
        .starting_session_servers
        .push(starting_session_server);

    finish_session_creation(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &build_unread_router_events_sender(),
        starting_session_id,
        Some(SessionServerReady {
            protocol_version: ROUTER_PROTOCOL_VERSION + 1,
            socket_address: compute_socket_address(runtime_directory.path(), starting_session_id),
        }),
    );

    assert_eq!(
        response_receiver.try_recv(),
        Ok(build_expected_refusal(&format!(
            "the koshi binary on disk speaks control-plane protocol version {} and this running \
             router speaks {ROUTER_PROTOCOL_VERSION}, so they are different builds; run: koshi \
             restart-servers",
            ROUTER_PROTOCOL_VERSION + 1
        )))
    );
    assert_eq!(router_sessions.session_registry, SessionRegistry::new());
    assert_eq!(router_sessions.starting_session_servers.len(), 0);
    assert!(is_process_collected(child_process_id));
}

#[cfg(unix)]
#[test]
fn a_due_restart_keeps_every_waiting_request_and_its_starting_session_server() {
    let starting_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (lookup_sender, lookup_receiver) = mpsc::channel();
    router_sessions
        .waiting_attach_lookups
        .push(WaitingAttachLookup {
            session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
            awaited_session_ids: BTreeSet::new(),
            unlisted_session_count: 0,
            unread_paths: Vec::new(),
            answer_deadline: Instant::now() + SESSION_DISCOVERY_TIMEOUT_DURATION,
            probed_session_id: None,
            response_sender: lookup_sender,
        });
    let (creation_sender, creation_receiver) = mpsc::channel();
    router_sessions
        .queued_session_creations
        .push(QueuedSessionCreation {
            profile: None,
            working_directory: None,
            is_other_user_access_allowed: None,
            response_sender: creation_sender,
        });
    let (starting_session_server, starting_receiver) = build_starting_session_server(
        starting_session_id,
        Instant::now() + SESSION_SERVER_READY_TIMEOUT_DURATION,
    );
    let child_process_id = starting_session_server.child_process.id();
    router_sessions
        .starting_session_servers
        .push(starting_session_server);

    serve_router_event(
        runtime_directory.path(),
        None,
        &build_test_executable_watch(),
        None,
        &mut router_sessions,
        &mut build_no_remote_state(),
        &build_unread_router_events_sender(),
        RouterEvent::RestartDue,
    );

    assert!(router_sessions.is_restart_pending);
    assert_eq!(lookup_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(creation_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(starting_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(router_sessions.waiting_attach_lookups.len(), 1);
    assert_eq!(router_sessions.queued_session_creations.len(), 1);
    assert!(!is_process_collected(child_process_id));
    let mut drained_session_server = router_sessions.starting_session_servers.remove(0);
    drained_session_server
        .child_process
        .kill()
        .expect("the child is killed");
    drained_session_server
        .child_process
        .wait()
        .expect("the child is reaped");
}

#[test]
fn a_request_that_would_wait_is_refused_while_a_restart_is_pending() {
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    router_sessions.is_restart_pending = true;
    let mut remote_state = build_remote_state_admitting_one_connection();
    let serve_request = |router_sessions: &mut RouterSessions,
                         remote_state: &mut RemoteState,
                         request_kind: RouterRequestKind| {
        let (response_sender, response_receiver) = mpsc::channel();
        serve_router_request(
            runtime_directory.path(),
            None,
            &build_test_executable_watch(),
            None,
            router_sessions,
            remote_state,
            &build_unread_router_events_sender(),
            request_kind,
            response_sender,
        );
        response_receiver.try_recv()
    };
    let restarting_refusal = build_expected_refusal(ROUTER_RESTARTING_MESSAGE);

    assert_eq!(
        serve_request(
            &mut router_sessions,
            &mut remote_state,
            RouterRequestKind::AttachLookup {
                session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
            }
        ),
        Ok(restarting_refusal.clone())
    );
    assert_eq!(
        serve_request(
            &mut router_sessions,
            &mut remote_state,
            RouterRequestKind::CreateSession {
                profile: None,
                working_directory: None,
                is_other_user_access_allowed: None,
            }
        ),
        Ok(restarting_refusal)
    );
    assert_eq!(
        serve_request(
            &mut router_sessions,
            &mut remote_state,
            RouterRequestKind::RemoteStatus
        ),
        Ok(build_remote_status_result(&remote_state)),
        "a request that needs no wait is answered"
    );
    let (endpoint_sender, endpoint_receiver) = mpsc::channel();
    serve_remote_admission(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &mut remote_state,
        &build_unread_router_events_sender(),
        AdmissionAsk::Locate {
            scope: TokenScope::HostWide,
            remote_connection_id: 0,
            session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
            response_sender: endpoint_sender,
        },
    );
    assert_eq!(
        endpoint_receiver.try_recv(),
        Ok(Err(LocateRefusal::RouterRestarting))
    );
    assert!(is_nothing_in_flight(&router_sessions));
}

#[test]
fn the_dispatcher_restarts_once_the_waiting_lookup_is_answered_after_a_due_restart() {
    // The lookup waits for a description due 300 ms after the loop starts.
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (lookup_sender, lookup_receiver) = mpsc::channel();
    let held_runtime_directory = runtime_directory.path().to_path_buf();
    let loop_started_at = Instant::now();
    let loop_thread = std::thread::spawn(move || {
        let (router_events_sender, router_events_receiver) = mpsc::channel();
        router_events_sender
            .send(RouterEvent::RestartDue)
            .expect("the restart is queued");
        let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
        insert_description_in_flight(
            &mut router_sessions,
            silent_session_id,
            Instant::now() + Duration::from_millis(300) - SESSION_DISCOVERY_TIMEOUT_DURATION,
        );
        router_sessions
            .waiting_attach_lookups
            .push(WaitingAttachLookup {
                session_selector: SessionSelector::SessionId(silent_session_id),
                awaited_session_ids: BTreeSet::from([silent_session_id]),
                unlisted_session_count: 0,
                unread_paths: Vec::new(),
                answer_deadline: Instant::now() + SESSION_DISCOVERY_TIMEOUT_DURATION,
                probed_session_id: None,
                response_sender: lookup_sender,
            });
        run_dispatch_loop(
            &held_runtime_directory,
            None,
            &build_test_executable_watch(),
            None,
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            TEST_LIVENESS_CHECK_INTERVAL_DURATION,
            &mut router_sessions,
            &mut build_no_remote_state(),
        )
    });

    assert_eq!(join_dispatch_loop_thread(loop_thread), RouterExit::Restart);
    assert!(
        loop_started_at.elapsed() >= Duration::from_millis(300),
        "the restart waits for the lookup"
    );
    assert_eq!(
        lookup_receiver.try_recv(),
        Ok(build_expected_refusal(&format!(
            "session {silent_session_id} is running but did not answer: it did not answer \
             within 5 seconds"
        )))
    );
}

#[cfg(unix)]
#[test]
fn the_dispatcher_restarts_once_the_starting_session_server_is_settled_after_a_due_restart() {
    // The session server never reports; its ready deadline is 300 ms after
    // the loop starts.
    let starting_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (starting_session_server, starting_receiver) = build_starting_session_server(
        starting_session_id,
        Instant::now() + Duration::from_millis(300),
    );
    let child_process_id = starting_session_server.child_process.id();
    let held_runtime_directory = runtime_directory.path().to_path_buf();
    let loop_thread = std::thread::spawn(move || {
        let (router_events_sender, router_events_receiver) = mpsc::channel();
        router_events_sender
            .send(RouterEvent::RestartDue)
            .expect("the restart is queued");
        let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
        router_sessions
            .starting_session_servers
            .push(starting_session_server);
        run_dispatch_loop(
            &held_runtime_directory,
            None,
            &build_test_executable_watch(),
            None,
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            TEST_LIVENESS_CHECK_INTERVAL_DURATION,
            &mut router_sessions,
            &mut build_no_remote_state(),
        )
    });

    assert_eq!(join_dispatch_loop_thread(loop_thread), RouterExit::Restart);
    assert_eq!(
        starting_receiver.try_recv(),
        Ok(build_expected_refusal(
            "the session did not report a bound socket"
        ))
    );
    assert!(is_process_collected(child_process_id));
}

#[cfg(unix)]
#[test]
fn a_starting_session_server_whose_ready_report_is_sent_is_kept_past_its_deadline() {
    let starting_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let (starting_session_server, starting_receiver) =
        build_starting_session_server(starting_session_id, Instant::now());
    starting_session_server
        .is_ready_report_sent
        .store(true, Ordering::SeqCst);
    let child_process_id = starting_session_server.child_process.id();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    router_sessions
        .starting_session_servers
        .push(starting_session_server);
    let now = Instant::now();

    advance_session_creations(
        runtime_directory.path(),
        None,
        &mut router_sessions,
        &build_unread_router_events_sender(),
        now,
    );

    assert_eq!(starting_receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(router_sessions.starting_session_servers.len(), 1);
    assert_eq!(
        find_next_waiting_request_wake_at(&router_sessions, now),
        Some(now)
    );
    assert!(!is_process_collected(child_process_id));
    let mut kept_session_server = router_sessions.starting_session_servers.remove(0);
    kept_session_server
        .child_process
        .kill()
        .expect("the child is killed");
    kept_session_server
        .child_process
        .wait()
        .expect("the child is reaped");
}

#[cfg(unix)]
#[test]
fn removing_a_session_removes_the_shared_socket_its_endpoint_file_names() {
    // The session bound under the shared directory while `allow-other-users`
    // was on, and its endpoint file names that socket.
    let gone_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let shared_user_directory = build_test_runtime_directory();
    let shared_socket_address =
        compute_socket_address(shared_user_directory.path(), gone_session_id);
    std::fs::write(&shared_socket_address, b"").expect("plant the leftover shared socket file");
    write_test_endpoint_file(
        runtime_directory.path(),
        gone_session_id,
        &shared_socket_address,
    );

    assert_eq!(
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut SessionRegistry::new(),
            gone_session_id,
            read_current_endpoint_file(runtime_directory.path(), gone_session_id).as_ref(),
        ),
        SessionRemoval::Removed
    );

    assert!(
        !Path::new(&shared_socket_address).exists(),
        "the shared socket file is removed"
    );
}

#[cfg(unix)]
#[test]
fn removing_a_session_leaves_a_shared_file_named_for_another_session_alone() {
    // The endpoint file names a socket whose file name is another session's.
    let gone_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let shared_user_directory = build_test_runtime_directory();
    let other_socket_address =
        compute_socket_address(shared_user_directory.path(), SessionId::new());
    std::fs::write(&other_socket_address, b"").expect("plant the other session's socket file");
    write_test_endpoint_file(
        runtime_directory.path(),
        gone_session_id,
        &other_socket_address,
    );

    assert_eq!(
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut SessionRegistry::new(),
            gone_session_id,
            read_current_endpoint_file(runtime_directory.path(), gone_session_id).as_ref(),
        ),
        SessionRemoval::Removed
    );

    assert!(
        Path::new(&other_socket_address).exists(),
        "a file named for another session is left alone"
    );
    assert!(
        !EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), gone_session_id)
            .exists(),
        "the endpoint file is removed"
    );
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

/// Answer one request the dispatcher answers at once, against an empty
/// session list, with `executable_watch` watching the binary a restart would
/// start, `token_store_path` as the remote access token store, and an events
/// channel nothing reads.
fn answer_router_request(
    runtime_directory: &Path,
    executable_watch: &Arc<ExecutableWatch>,
    token_store_path: Option<&Path>,
    request_kind: RouterRequestKind,
) -> RouterResult {
    let (response_sender, response_receiver) = mpsc::channel();
    serve_router_request(
        runtime_directory,
        None,
        executable_watch,
        token_store_path,
        &mut RouterSessions::from_session_registry(SessionRegistry::new()),
        &mut build_no_remote_state(),
        &build_unread_router_events_sender(),
        request_kind,
        response_sender,
    );
    response_receiver
        .try_recv()
        .expect("the request is answered at once")
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

    let router_result = answer_router_request(
        runtime_directory.path(),
        &Arc::new(ExecutableWatch::new(executable_path.clone(), BUILD_VERSION)),
        None,
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

    let router_result = answer_router_request(
        runtime_directory.path(),
        &Arc::new(ExecutableWatch::new(executable_path.clone(), BUILD_VERSION)),
        None,
        RouterRequestKind::Restart,
    );

    assert_eq!(
        router_result,
        build_expected_refusal(&format!(
            "the binary at {} could not be read: {metadata_error}",
            executable_path.display()
        ))
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

    let router_result = answer_router_request(
        runtime_directory.path(),
        &Arc::new(ExecutableWatch::new(executable_path.clone(), BUILD_VERSION)),
        None,
        RouterRequestKind::Restart,
    );

    assert_eq!(
        router_result,
        build_expected_refusal(&format!(
            "the binary at {} is not executable",
            executable_path.display()
        ))
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

/// The reaper waits on the session server the router started and sends
/// `ChildExited` with the session id once it exits. The reaper thread holds the
/// only other sender: an event or a closed channel ends the wait.
#[cfg(unix)]
#[test]
fn the_reaper_reports_the_exit_of_the_session_server_it_started() {
    let session_id = SessionId::new();
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    let has_exit_watcher =
        start_session_reaper_thread(spawn_short_lived_child(), session_id, router_events_sender);

    assert!(has_exit_watcher, "the reaper thread started");
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

/// Register the session `session_id` from its description, as the dispatcher
/// does at startup: a stand-in session server answers at the session's socket
/// in `runtime_directory`, its endpoint file names `session_process_id`, and
/// the answer is served on `router_sessions`. Hands back the socket address.
#[cfg(unix)]
fn register_described_session(
    runtime_directory: &Path,
    session_id: SessionId,
    session_process_id: u32,
    router_sessions: &mut RouterSessions,
    router_events_sender: &Sender<RouterEvent>,
    router_events_receiver: &Receiver<RouterEvent>,
) -> String {
    let socket_address = compute_socket_address(runtime_directory, session_id);
    let session_server_thread = spawn_session_server_answering_one_connection(
        &socket_address,
        session_id,
        "S-quiet-lake",
        UNIX_EPOCH + Duration::from_secs(1_700_000_000),
    );
    write_test_endpoint_file_for_process(
        runtime_directory,
        session_id,
        &socket_address,
        session_process_id,
    );
    start_session_description(
        runtime_directory,
        router_sessions,
        router_events_sender,
        session_id,
        DescribedSessionOrigin::ThisUser,
    );
    let described_event = router_events_receiver
        .recv_timeout(LOOP_END_TIMEOUT_DURATION)
        .expect("the answer is reported");
    serve_router_event(
        runtime_directory,
        None,
        &build_test_executable_watch(),
        None,
        router_sessions,
        &mut build_no_remote_state(),
        router_events_sender,
        described_event,
    );
    drop(
        session_server_thread
            .join()
            .expect("the stand-in session ended"),
    );
    socket_address
}

/// A session registered from its description, whose process an earlier image
/// of this router started, is watched from its registration. The thread that
/// adopted the process reaps it and reports its exit, and the dispatcher then
/// probes the session.
#[cfg(unix)]
#[test]
fn a_session_described_with_an_inherited_process_reports_its_exit() {
    let inherited_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut session_process = spawn_running_child("sleep 30");
    let session_process_id = session_process.id();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    adopt_inherited_children(
        vec![session_process_id],
        &mut router_sessions,
        &router_events_sender,
    );

    let socket_address = register_described_session(
        runtime_directory.path(),
        inherited_session_id,
        session_process_id,
        &mut router_sessions,
        &router_events_sender,
        &router_events_receiver,
    );

    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            inherited_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: session_process_id,
                has_exit_watcher: true,
            },
        )]),
        "a session whose process is an adopted child is watched"
    );
    session_process
        .kill()
        .expect("the session process is ended");
    let exit_event = router_events_receiver
        .recv_timeout(LOOP_END_TIMEOUT_DURATION)
        .expect("the exit is reported");
    match &exit_event {
        RouterEvent::ChildProcessReaped { process_id } => {
            assert_eq!(*process_id, session_process_id)
        }
        _ => panic!("the adopting thread reported something other than the exit"),
    }
    serve_router_event(
        runtime_directory.path(),
        None,
        &build_test_executable_watch(),
        None,
        &mut router_sessions,
        &mut build_no_remote_state(),
        &router_events_sender,
        exit_event,
    );

    assert_eq!(
        (
            router_sessions.session_registry[&inherited_session_id].has_exit_watcher,
            router_sessions
                .probing_session_ids
                .contains(&inherited_session_id),
            router_sessions.inherited_child_process_ids.clone(),
        ),
        (false, true, BTreeSet::new()),
        "the exit leaves the session unwatched and probed"
    );
    assert_eq!(
        session_process
            .try_wait()
            .err()
            .map(|wait_error| wait_error.raw_os_error()),
        Some(Some(libc::ECHILD)),
        "the adopting thread reaped the process"
    );
}

/// A session registered from its description, whose process no thread of
/// this router waits on, has no exit watcher: the liveness checks probe it.
#[cfg(unix)]
#[test]
fn a_session_described_with_a_process_no_thread_waits_on_has_no_exit_watcher() {
    let described_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut session_process = spawn_running_child("sleep 30");
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    let socket_address = register_described_session(
        runtime_directory.path(),
        described_session_id,
        session_process.id(),
        &mut router_sessions,
        &router_events_sender,
        &router_events_receiver,
    );

    assert_eq!(
        router_sessions.session_registry,
        SessionRegistry::from([(
            described_session_id,
            SessionRecord {
                session_name: "S-quiet-lake".to_string(),
                socket_address,
                process_id: session_process.id(),
                has_exit_watcher: false,
            },
        )]),
        "a session whose process no thread waits on is not watched"
    );
    terminate_child_process(&mut session_process);
}

/// A child an earlier image started that serves no listed session, such as a
/// session that ended while this image started, is reaped once it exits. Its
/// exit changes no session.
#[cfg(unix)]
#[test]
fn an_inherited_child_that_serves_no_session_is_reaped_once_it_exits() {
    let mut ended_session_process = spawn_short_lived_child();
    let ended_process_id = ended_session_process.id();
    let runtime_directory = build_test_runtime_directory();
    let listed_session_id = SessionId::new();
    let mut router_sessions = RouterSessions::from_session_registry(build_session_registry(&[(
        listed_session_id,
        "S-quiet-lake",
    )]));
    let listed_registry = router_sessions.session_registry.clone();
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    adopt_inherited_children(
        vec![ended_process_id],
        &mut router_sessions,
        &router_events_sender,
    );
    assert_eq!(
        router_sessions.inherited_child_process_ids,
        BTreeSet::from([ended_process_id]),
        "the child is adopted"
    );
    let exit_event = router_events_receiver
        .recv_timeout(LOOP_END_TIMEOUT_DURATION)
        .expect("the exit is reported");
    serve_router_event(
        runtime_directory.path(),
        None,
        &build_test_executable_watch(),
        None,
        &mut router_sessions,
        &mut build_no_remote_state(),
        &router_events_sender,
        exit_event,
    );

    assert_eq!(
        (
            router_sessions.session_registry.clone(),
            router_sessions.probing_session_ids.clone(),
            router_sessions.inherited_child_process_ids.clone(),
        ),
        (listed_registry, BTreeSet::new(), BTreeSet::new()),
        "no session changes and nothing is probed"
    );
    assert_eq!(
        ended_session_process
            .try_wait()
            .err()
            .map(|wait_error| wait_error.raw_os_error()),
        Some(Some(libc::ECHILD)),
        "the adopting thread reaped the child"
    );
}

/// The reap drops each exited child and each id that names no child of this
/// process, and keeps each child that still runs.
#[cfg(unix)]
#[test]
fn reaping_unwaited_children_drops_each_exited_child_and_keeps_each_running_one() {
    let mut exited_child = spawn_short_lived_child();
    wait_until_child_has_exited(exited_child.id());
    let mut running_child = spawn_running_child("sleep 30");
    let parent_process_id = std::os::unix::process::parent_id();
    let mut unwaited_child_process_ids =
        BTreeSet::from([0, exited_child.id(), running_child.id(), parent_process_id]);

    reap_unwaited_children(&mut unwaited_child_process_ids)
        .expect("unwaited children can be reaped");

    assert_eq!(
        unwaited_child_process_ids,
        BTreeSet::from([running_child.id()]),
        "only the running child stays"
    );
    assert_eq!(
        exited_child
            .try_wait()
            .err()
            .map(|wait_error| wait_error.raw_os_error()),
        Some(Some(libc::ECHILD)),
        "the reap collected the exited child"
    );
    terminate_child_process(&mut running_child);
}

/// An empty list ends the loop at its idle window while a child that no
/// thread waits on still runs: a child process keeps no loop running. The
/// child stays in the set.
#[cfg(unix)]
#[test]
fn a_running_unwaited_child_does_not_keep_an_empty_loop_past_the_idle_window() {
    let mut running_child = spawn_running_child("sleep 30");
    let running_process_id = running_child.id();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    router_sessions
        .unwaited_child_process_ids
        .insert(running_process_id);
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    let router_exit = run_dispatch_loop(
        runtime_directory.path(),
        None,
        &build_test_executable_watch(),
        None,
        &router_events_sender,
        &router_events_receiver,
        TEST_IDLE_EXIT_DURATION,
        TEST_LIVENESS_CHECK_INTERVAL_DURATION,
        &mut router_sessions,
        &mut build_no_remote_state(),
    );
    let remaining_unwaited_child_process_ids = router_sessions.unwaited_child_process_ids.clone();
    terminate_child_process(&mut running_child);

    assert_eq!(
        (router_exit, remaining_unwaited_child_process_ids),
        (RouterExit::Idle, BTreeSet::from([running_process_id])),
        "the empty loop ends idle and the running child stays in the set"
    );
}

/// An empty list ends the loop at its idle window while an adopted child of
/// an earlier image still runs: the thread that waits on that child keeps no
/// loop running. The child stays in the set.
#[cfg(unix)]
#[test]
fn a_running_inherited_child_does_not_keep_an_empty_loop_past_the_idle_window() {
    let mut running_child = spawn_running_child("sleep 30");
    let running_process_id = running_child.id();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    adopt_inherited_children(
        vec![running_process_id],
        &mut router_sessions,
        &router_events_sender,
    );

    let router_exit = run_dispatch_loop(
        runtime_directory.path(),
        None,
        &build_test_executable_watch(),
        None,
        &router_events_sender,
        &router_events_receiver,
        TEST_IDLE_EXIT_DURATION,
        TEST_LIVENESS_CHECK_INTERVAL_DURATION,
        &mut router_sessions,
        &mut build_no_remote_state(),
    );
    let remaining_inherited_child_process_ids = router_sessions.inherited_child_process_ids.clone();
    running_child.kill().expect("the running child is ended");
    let exit_event = router_events_receiver
        .recv_timeout(LOOP_END_TIMEOUT_DURATION)
        .expect("the waiting thread reports the exit");

    assert_eq!(
        (router_exit, remaining_inherited_child_process_ids),
        (RouterExit::Idle, BTreeSet::from([running_process_id])),
        "the empty loop ends idle and the running child stays adopted"
    );
    match exit_event {
        RouterEvent::ChildProcessReaped { process_id } => {
            assert_eq!(process_id, running_process_id)
        }
        _ => panic!("the waiting thread reported something other than the exit"),
    }
    assert_eq!(
        running_child
            .try_wait()
            .err()
            .map(|wait_error| wait_error.raw_os_error()),
        Some(Some(libc::ECHILD)),
        "the waiting thread reaped the child"
    );
}

/// A liveness round reaps an exited child that no thread waits on while a
/// session is listed. The round reaps before it probes, so the child is reaped
/// before the probe drops the gone session and the loop ends idle.
#[cfg(unix)]
#[test]
fn a_liveness_round_reaps_an_exited_unwaited_child_while_a_session_is_listed() {
    let mut exited_child = spawn_short_lived_child();
    wait_until_child_has_exited(exited_child.id());
    let exited_process_id = exited_child.id();
    let gone_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::from([(
        gone_session_id,
        build_unwatched_session_record(compute_socket_address(
            runtime_directory.path(),
            gone_session_id,
        )),
    )]));
    router_sessions
        .unwaited_child_process_ids
        .insert(exited_process_id);
    let (router_events_sender, router_events_receiver) = mpsc::channel();

    let router_exit = run_dispatch_loop(
        runtime_directory.path(),
        None,
        &build_test_executable_watch(),
        None,
        &router_events_sender,
        &router_events_receiver,
        TEST_IDLE_EXIT_DURATION,
        TEST_LIVENESS_CHECK_INTERVAL_DURATION,
        &mut router_sessions,
        &mut build_no_remote_state(),
    );

    assert_eq!(
        (
            router_exit,
            router_sessions.session_registry.clone(),
            router_sessions.unwaited_child_process_ids.clone()
        ),
        (RouterExit::Idle, SessionRegistry::new(), BTreeSet::new()),
        "the round reaped the child, and the probe dropped the gone session"
    );
    assert_eq!(
        exited_child
            .try_wait()
            .err()
            .map(|wait_error| wait_error.raw_os_error()),
        Some(Some(libc::ECHILD)),
        "the liveness round collected the exited child"
    );
}

/// The router hands its place over by starting the new binary with this
/// argument, the one [`crate::cli`] parses into `wait_for_lock`. On Windows the
/// two creation flags the handover carries are checked beside them, in
/// [`crate::process`].
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

#[test]
fn a_connection_to_a_router_whose_program_file_holds_another_version_makes_the_restart_due() {
    let runtime_directory = build_test_runtime_directory();
    let program_path = write_printing_program(
        runtime_directory.path(),
        "koshi",
        &runtime_directory.path().join("started_runs"),
        "koshi 1.0.0",
    );
    let executable_watch = Arc::new(ExecutableWatch::new(program_path.clone(), "1.0.0"));
    let replacement_path = write_printing_program(
        runtime_directory.path(),
        "replacement",
        &runtime_directory.path().join("runs"),
        "koshi 9.9.9",
    );
    std::fs::rename(&replacement_path, &program_path).expect("the program file is replaced");
    let router_socket_address = compute_router_socket_address(runtime_directory.path());
    let router_listener =
        Listener::bind(&router_socket_address).expect("the router socket is bound");
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let is_shutting_down = Arc::new(AtomicBool::new(false));
    let accept_loop_shutdown_flag = Arc::clone(&is_shutting_down);
    let accept_thread = std::thread::spawn(move || {
        run_router_accept_loop(
            &router_listener,
            &ConnectionToken::generate(),
            &router_events_sender,
            &accept_loop_shutdown_flag,
            &executable_watch,
        );
    });

    let caller_connection =
        Connection::connect(&router_socket_address).expect("the caller reaches the router");

    let router_event = router_events_receiver.recv_timeout(Duration::from_secs(10));
    let Ok(RouterEvent::RestartDue) = router_event else {
        panic!("the dispatcher was not told the restart is due");
    };
    is_shutting_down.store(true, Ordering::SeqCst);
    drop(caller_connection);
    let _ = Connection::connect(&router_socket_address);
    accept_thread.join().expect("the accept loop ends");
}

#[test]
fn a_hello_refused_for_its_version_makes_the_restart_due_when_the_program_file_holds_another_version(
) {
    let runtime_directory = build_test_runtime_directory();
    let run_log_path = runtime_directory.path().join("runs");
    let program_path = write_printing_program(
        runtime_directory.path(),
        "koshi",
        &run_log_path,
        "koshi 9.9.9",
    );
    let executable_watch = Arc::new(ExecutableWatch::new(program_path, "1.0.0"));
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
            &executable_watch,
        );
    });
    let mut caller_connection =
        Connection::connect(&router_socket_address).expect("the caller reaches the router");
    assert_eq!(
        router_events_receiver
            .recv_timeout(Duration::from_millis(500))
            .err(),
        Some(mpsc::RecvTimeoutError::Timeout)
    );
    assert_eq!(count_program_runs(&run_log_path), 0);

    caller_connection
        .send(&RouterRequest {
            request_id: 1,
            request_kind: RouterRequestKind::Hello {
                minimum_protocol_version: ROUTER_PROTOCOL_VERSION + 1,
                maximum_protocol_version: ROUTER_PROTOCOL_VERSION + 2,
                connection_token: router_connection_token,
            },
        })
        .expect("the hello is written");
    let hello_response: RouterResponse = caller_connection.recv().expect("the hello is answered");

    let RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::UnsupportedVersion,
        ..
    }) = hello_response.answer_result
    else {
        panic!("expected a version refusal, got {hello_response:?}");
    };
    let router_event = router_events_receiver.recv_timeout(Duration::from_secs(10));
    let Ok(RouterEvent::RestartDue) = router_event else {
        panic!("the dispatcher was not told the restart is due");
    };
    assert_eq!(count_program_runs(&run_log_path), 1);
    is_shutting_down.store(true, Ordering::SeqCst);
    drop(caller_connection);
    let _ = Connection::connect(&router_socket_address);
    accept_thread.join().expect("the accept loop ends");
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
            &build_unread_executable_watch(),
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
            request_kind: RouterRequestKind::RemoteStatus,
        })
        .expect("the status request is written");
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
    assert_eq!(request_kind, RouterRequestKind::RemoteStatus);

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
    answer_router_request(
        runtime_directory.path(),
        &build_test_executable_watch(),
        token_store_path,
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
/// JSON: a byte comparison against the returned bytes fails after any
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
    let refusal_result = build_expected_refusal(
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
    let refusal_result = build_expected_refusal(&token_store_error.to_string());

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
    let refusal_result = build_expected_refusal(
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
             router speaks {ROUTER_PROTOCOL_VERSION}, so they are different builds; run: koshi \
             restart-servers",
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
fn find_free_loopback_address() -> SocketAddr {
    let probe_listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    probe_listener
        .local_addr()
        .expect("the address that was bound")
}

/// Open the real remote listener on a loopback port, trying up to
/// [`MAX_ADDRESS_ATTEMPT_COUNT`] ports, and hand back the address it bound.
fn open_test_remote_listener(
    certificate_file: &CertificateFile,
    router_events_sender: &Sender<RouterEvent>,
) -> SocketAddr {
    for _ in 0..MAX_ADDRESS_ATTEMPT_COUNT {
        let loopback_address = find_free_loopback_address();
        let bound_listener_result =
            remote_listener::bind_remote_listener(loopback_address, certificate_file);
        if let Ok(bound_listener) = bound_listener_result {
            bound_listener.start_serving(
                router_events_sender.clone(),
                build_unread_executable_watch(),
            );
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
        let remote_enable_result = enable_remote_access(
            remote_state,
            &build_unread_executable_watch(),
            router_events_sender,
        );
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
    let listed_socket_address = socket_address.clone();
    EndpointFile {
        socket_address,
        connection_token: ConnectionToken::generate(),
        process_id: NO_SUCH_PROCESS_ID,
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
    let held_data_directory = data_directory.clone();
    let loop_thread = std::thread::spawn(move || {
        // The session is listed at the address its stand-in serves.
        let mut session_registry = build_session_registry(&[(session_id, "S-quiet-lake")]);
        session_registry
            .get_mut(&session_id)
            .expect("the session is listed")
            .socket_address = listed_socket_address;
        let mut remote_state = RemoteState {
            remote_listen_address: Some(remote_listen_address),
            data_directory: Some(held_data_directory),
            is_listening: true,
            admitted_remote_connections: Vec::new(),
            next_remote_connection_id: 0,
            full_capacity_warning: WarningRateLimiter::new(),
        };
        run_dispatch_loop(
            &held_runtime_directory,
            None,
            &build_test_executable_watch(),
            Some(&held_token_store_path),
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            TEST_LIVENESS_CHECK_INTERVAL_DURATION,
            &mut RouterSessions::from_session_registry(session_registry),
            &mut remote_state,
        )
    });

    // The first connection attaches to the session through the router's
    // bridge.
    let attaching_connection = remote_client::connect_remote_server(
        &remote_listen_address.to_string(),
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
        &remote_listen_address.to_string(),
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

    // The stand-in session ends, and then its exit is reported. Nothing listens
    // at its address, so the router removes it and goes idle.
    drop(stop_sender);
    session_server_thread
        .join()
        .expect("the stand-in session ended");
    test_router_event_sender
        .send(RouterEvent::ChildExited(session_id))
        .expect("the exit is queued");
    assert_eq!(join_dispatch_loop_thread(loop_thread), RouterExit::Idle);
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
            has_exit_watcher: false,
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
        &mut RouterSessions::from_session_registry(SessionRegistry::new()),
        &mut remote_state,
        &build_unread_router_events_sender(),
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
            &remote_listen_address.to_string(),
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
    let held_data_directory = data_directory.clone();
    let loop_thread = std::thread::spawn(move || {
        let session_registry = build_session_registry(&[]);
        let mut remote_state = RemoteState {
            remote_listen_address: Some(remote_listen_address),
            data_directory: Some(held_data_directory),
            is_listening: true,
            admitted_remote_connections: Vec::new(),
            next_remote_connection_id: 0,
            full_capacity_warning: WarningRateLimiter::new(),
        };
        run_dispatch_loop(
            &held_runtime_directory,
            None,
            &build_test_executable_watch(),
            Some(&held_token_store_path),
            &router_events_sender,
            &router_events_receiver,
            TEST_IDLE_EXIT_DURATION,
            TEST_LIVENESS_CHECK_INTERVAL_DURATION,
            &mut RouterSessions::from_session_registry(session_registry),
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
            &remote_listen_address.to_string(),
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
        &mut RouterSessions::from_session_registry(SessionRegistry::new()),
        &mut remote_state,
        &build_unread_router_events_sender(),
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

    let enable_result = enable_remote_access(
        &mut remote_state,
        &build_unread_executable_watch(),
        &router_events_sender,
    );

    assert_eq!(
        enable_result,
        build_expected_refusal(
            "no remote listen address is set; add `remote-listen \"<ip>:<port>\"` to koshi.kdl"
        )
    );
    assert!(!remote_state.is_listening, "no port is open");
}

#[test]
fn switching_remote_access_on_with_no_data_directory_is_refused() {
    // The certificate and the remote access record both live in the data
    // directory. A machine with no data directory is refused.
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = build_no_remote_state();
    remote_state.remote_listen_address = Some(SocketAddr::from(([127, 0, 0, 1], 7654)));

    let enable_result = enable_remote_access(
        &mut remote_state,
        &build_unread_executable_watch(),
        &router_events_sender,
    );

    assert_eq!(
        enable_result,
        build_expected_refusal(
            "this machine has no data directory, so remote access cannot be switched on"
        )
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
        .expect("read the held address");
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = RemoteState {
        remote_listen_address: Some(remote_listen_address),
        data_directory: Some(data_directory.clone()),
        is_listening: true,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };

    let enable_result = enable_remote_access(
        &mut remote_state,
        &build_unread_executable_watch(),
        &router_events_sender,
    );

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
        is_remote_access_enabled(&data_directory),
        "the remote access record is written in the data directory"
    );

    drop(occupied_listener);
}

#[test]
fn the_start_up_open_with_no_listen_address_takes_no_port() {
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = build_no_remote_state();

    open_remote_listener(
        &mut remote_state,
        &build_unread_executable_watch(),
        &router_events_sender,
    );

    assert!(!remote_state.is_listening, "no address opens no port");
}

#[test]
fn the_start_up_open_takes_no_port_until_the_operator_has_said_yes() {
    // An address alone opens nothing: the start opens the port only when the
    // remote access record sits in the data directory.
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

    open_remote_listener(
        &mut remote_state,
        &build_unread_executable_watch(),
        &router_events_sender,
    );

    assert!(
        !remote_state.is_listening,
        "no remote access record, so no port"
    );
    assert!(
        !CertificateFile::resolve_certificate_file_path(&data_directory).exists(),
        "the open stopped before it made this machine's certificate"
    );
}

#[test]
fn the_start_up_open_takes_the_port_again_once_the_answer_is_written_down() {
    // The remote access record beside the certificate opens the port on every
    // start after the one that wrote it, with nobody asked again.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    RemoteAccessRecord {
        file_format: REMOTE_ACCESS_RECORD_FILE_FORMAT,
        enabled_at: SystemTime::now(),
    }
    .write_to_path(&RemoteAccessRecord::resolve_remote_access_record_path(
        &data_directory,
    ))
    .expect("the remote access record is written");
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
        open_remote_listener(
            &mut remote_state,
            &build_unread_executable_watch(),
            &router_events_sender,
        );
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
        .expect("the address it took");
    assert_eq!(
        TcpListener::bind(remote_listen_address)
            .expect_err("the router is holding the address")
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
}

#[test]
fn an_address_that_cannot_be_taken_writes_no_record_of_the_answer() {
    // `enable_remote_access` binds the port before it writes the remote access record.
    // An address another socket holds is refused, and no remote access record is
    // written.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");

    let occupied_listener = TcpListener::bind("127.0.0.1:0").expect("hold a loopback address");
    let occupied_socket_address = occupied_listener
        .local_addr()
        .expect("read the held address");

    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut remote_state = RemoteState {
        remote_listen_address: Some(occupied_socket_address),
        data_directory: Some(data_directory.clone()),
        is_listening: false,
        admitted_remote_connections: Vec::new(),
        next_remote_connection_id: 0,
        full_capacity_warning: WarningRateLimiter::new(),
    };

    let bind_error =
        TcpListener::bind(occupied_socket_address).expect_err("the address is already held");
    let enable_result = enable_remote_access(
        &mut remote_state,
        &build_unread_executable_watch(),
        &router_events_sender,
    );

    assert_eq!(
        enable_result,
        build_expected_refusal(&format!(
            "the remote listener could not open {occupied_socket_address}: {bind_error}"
        ))
    );
    assert!(
        !remote_state.is_listening,
        "nothing is being served on an address that was never taken"
    );
    assert!(
        !RemoteAccessRecord::resolve_remote_access_record_path(&data_directory).exists(),
        "the remote access record was never written"
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
        is_remote_access_enabled(&data_directory),
        "the remote access record is written in the data directory"
    );
}

#[test]
fn the_status_separates_the_answer_given_from_the_port_being_open() {
    // The remote access record exists and no port is open: the status reports
    // `is_remote_access_enabled` as `true` and `is_listening` as `false`.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    RemoteAccessRecord {
        file_format: REMOTE_ACCESS_RECORD_FILE_FORMAT,
        enabled_at: SystemTime::now(),
    }
    .write_to_path(&RemoteAccessRecord::resolve_remote_access_record_path(
        &data_directory,
    ))
    .expect("the remote access record is written");

    let remote_state = RemoteState {
        remote_listen_address: Some(SocketAddr::from(([127, 0, 0, 1], 7654))),
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
    // connections. A certificate with no remote access record reports
    // `is_remote_access_enabled` as `false`.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let (_certificate_file, certificate_fingerprint) =
        load_or_create_certificate(&data_directory).expect("this machine's certificate");
    let mut remote_state = RemoteState {
        remote_listen_address: Some(SocketAddr::from(([127, 0, 0, 1], 7654))),
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
            remote_listen_address: Some(SocketAddr::from(([127, 0, 0, 1], 7654))),
            is_remote_access_enabled: false,
            is_listening: true,
            certificate_fingerprint: Some(certificate_fingerprint),
            remote_connection_count: 3,
        }
    );
}

#[test]
fn a_bound_port_that_is_never_served_is_given_back() {
    // `enable_remote_access` binds the port, writes the remote access record, then
    // starts serving. A failed write drops the bound listener: the same address
    // binds again after the drop.
    let runtime_directory = build_test_runtime_directory();
    let data_directory = runtime_directory.path().join("data");
    let (certificate_file, _) =
        load_or_create_certificate(&data_directory).expect("this machine's certificate");
    let remote_listen_address = find_free_loopback_address();

    let bound_listener =
        remote_listener::bind_remote_listener(remote_listen_address, &certificate_file)
            .expect("the port is taken");
    drop(bound_listener);

    // The accept thread the bind started ends once its sender drops, and
    // releases the port.
    let mut is_port_free = false;
    for _ in 0..50 {
        if TcpListener::bind(remote_listen_address).is_ok() {
            is_port_free = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(is_port_free, "a port that was never served is free again");
}

#[test]
fn a_removal_finds_the_endpoint_file_rewritten_since_the_caller_read_it_and_keeps_the_session() {
    // The caller read a file holding one connection token, and the file on
    // disk now holds another. A caller that read no file finds one there now.
    let rebound_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    write_test_endpoint_file(
        runtime_directory.path(),
        rebound_session_id,
        &compute_socket_address(runtime_directory.path(), rebound_session_id),
    );
    let read_before_rebind_endpoint_file = EndpointFile {
        socket_address: compute_socket_address(runtime_directory.path(), rebound_session_id),
        connection_token: ConnectionToken::from_secret("a".repeat(64)),
        process_id: NO_SUCH_PROCESS_ID,
    };
    let mut session_registry = build_session_registry(&[(rebound_session_id, "S-quiet-lake")]);

    let session_removals = [
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut session_registry,
            rebound_session_id,
            Some(&read_before_rebind_endpoint_file),
        ),
        remove_session_from_registry(
            runtime_directory.path(),
            None,
            &mut session_registry,
            rebound_session_id,
            None,
        ),
    ];

    assert_eq!(
        session_removals,
        [SessionRemoval::Rebound, SessionRemoval::Rebound]
    );
    assert_eq!(
        session_registry,
        build_session_registry(&[(rebound_session_id, "S-quiet-lake")])
    );
    assert!(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), rebound_session_id)
            .exists(),
        "the endpoint file is left in place"
    );
}

#[test]
fn the_description_scan_records_a_session_between_its_two_images_without_asking_it() {
    // A resume file written a second ago, and no endpoint file.
    let restarting_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    build_aged_resume_file(
        runtime_directory.path(),
        restarting_session_id,
        INSIDE_RESTART_WINDOW_DURATION,
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let surveyed_session_ids = start_unlisted_session_descriptions(
        runtime_directory.path(),
        &mut router_sessions,
        &build_unread_router_events_sender(),
        ipc_client::list_own_sessions(runtime_directory.path())
            .expect("read the runtime directory"),
        ForeignSessionListing::default(),
    );

    assert_eq!(
        surveyed_session_ids,
        BTreeSet::from([restarting_session_id])
    );
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::new()
    );
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, restarting_session_id),
        Some("it is restarting".to_string())
    );
}

#[test]
fn the_description_scan_records_an_id_two_sockets_advertise_without_asking_either() {
    let duplicated_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let surveyed_session_ids = start_unlisted_session_descriptions(
        runtime_directory.path(),
        &mut router_sessions,
        &build_unread_router_events_sender(),
        ipc_client::list_own_sessions(runtime_directory.path())
            .expect("read the runtime directory"),
        ForeignSessionListing {
            duplicated_sessions: vec![DuplicatedForeignSession {
                session_id: duplicated_session_id,
                advertisement_count: 2,
                owner_user_ids: vec![1001, 1002],
            }],
            ..ForeignSessionListing::default()
        },
    );

    assert_eq!(
        surveyed_session_ids,
        BTreeSet::from([duplicated_session_id])
    );
    assert_eq!(
        list_describing_session_ids(&router_sessions),
        BTreeSet::new()
    );
    assert_eq!(
        find_unanswered_session_reason(&router_sessions, duplicated_session_id),
        Some(format!(
            "session {duplicated_session_id} is advertised 2 times in the shared directory, by \
             user ids 1001, 1002; koshi reaches none of them"
        ))
    );
}

#[test]
fn a_reason_a_waiting_lookup_awaits_outlives_a_scan_that_lists_no_other_user() {
    // A remote listing scans with no shared directory. The reason of the
    // session a waiting lookup awaits stays, and the other reason goes.
    let awaited_session_id = SessionId::new();
    let forgotten_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    for recorded_session_id in [awaited_session_id, forgotten_session_id] {
        router_sessions.unanswered_reason_by_session_id.insert(
            recorded_session_id,
            UnansweredSessionReason::TooManyOtherUserDescriptions,
        );
    }
    router_sessions
        .waiting_attach_lookups
        .push(WaitingAttachLookup {
            session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
            awaited_session_ids: BTreeSet::from([awaited_session_id]),
            unlisted_session_count: 0,
            unread_paths: Vec::new(),
            answer_deadline: Instant::now() + SESSION_DISCOVERY_TIMEOUT_DURATION,
            probed_session_id: None,
            response_sender: mpsc::channel().0,
        });

    start_unlisted_session_descriptions(
        runtime_directory.path(),
        &mut router_sessions,
        &build_unread_router_events_sender(),
        ipc_client::list_own_sessions(runtime_directory.path())
            .expect("read the runtime directory"),
        ForeignSessionListing::default(),
    );

    assert_eq!(
        router_sessions
            .unanswered_reason_by_session_id
            .keys()
            .copied()
            .collect::<BTreeSet<SessionId>>(),
        BTreeSet::from([awaited_session_id])
    );
}

/// Move a lookup of `session_selector` on once, over `router_sessions`,
/// awaiting `awaited_session_ids` with `unlisted_session_count` unlisted
/// sessions and `unread_paths` unread, and hand back its answer.
fn advance_lookup_once(
    runtime_directory: &Path,
    router_sessions: &mut RouterSessions,
    session_selector: SessionSelector,
    awaited_session_ids: BTreeSet<SessionId>,
    unlisted_session_count: usize,
    unread_paths: Vec<UnreadPath>,
) -> RouterResult {
    let (response_sender, response_receiver) = mpsc::channel();
    let now = Instant::now();
    let still_waiting_lookup = advance_attach_lookup(
        runtime_directory,
        router_sessions,
        &build_unread_router_events_sender(),
        WaitingAttachLookup {
            session_selector,
            awaited_session_ids,
            unlisted_session_count,
            unread_paths,
            answer_deadline: now,
            probed_session_id: None,
            response_sender,
        },
        now,
    );
    assert!(still_waiting_lookup.is_none(), "the lookup is answered");
    response_receiver
        .try_recv()
        .expect("the lookup sent its answer")
}

#[test]
fn a_name_one_other_users_session_carries_is_refused_while_another_session_did_not_answer() {
    let foreign_session_id = SessionId::new();
    let silent_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::from([(
        foreign_session_id,
        build_foreign_session_record(foreign_session_id, "S-quiet-lake"),
    )]));
    router_sessions
        .unanswered_reason_by_session_id
        .insert(silent_session_id, UnansweredSessionReason::NoAnswerInTime);

    let answer_with_a_silent_session = advance_lookup_once(
        runtime_directory.path(),
        &mut router_sessions,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
        BTreeSet::from([silent_session_id]),
        0,
        Vec::new(),
    );
    let answer_with_unlisted_sessions = advance_lookup_once(
        runtime_directory.path(),
        &mut router_sessions,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
        BTreeSet::new(),
        2,
        Vec::new(),
    );
    let answer_with_an_unread_path = advance_lookup_once(
        runtime_directory.path(),
        &mut router_sessions,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
        BTreeSet::new(),
        0,
        vec![build_unread_shared_folder()],
    );

    assert_eq!(
        answer_with_a_silent_session,
        build_expected_refusal(
            "cannot tell whether `S-quiet-lake` is unique (1 running session did not answer)"
        )
    );
    assert_eq!(
        answer_with_unlisted_sessions,
        build_expected_refusal(
            "cannot tell whether `S-quiet-lake` is unique (2 running sessions did not answer)"
        )
    );
    assert_eq!(
        answer_with_an_unread_path,
        build_expected_refusal(
            "cannot tell whether `S-quiet-lake` is unique (/home/user/koshi-shared/1002 could \
             not be read: Input/output error (os error 5))"
        )
    );
    assert_eq!(router_sessions.probing_session_ids, BTreeSet::new());
}

/// An unread folder of the shared directory: `/home/user/koshi-shared/1002`,
/// whose read failed with `Input/output error (os error 5)`.
fn build_unread_shared_folder() -> UnreadPath {
    UnreadPath {
        looked_up_path: PathBuf::from("/home/user/koshi-shared/1002"),
        read_error_text: "Input/output error (os error 5)".to_string(),
    }
}

#[test]
fn a_name_no_listed_session_carries_counts_the_unlisted_sessions_as_unanswered() {
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let lookup_answer = advance_lookup_once(
        runtime_directory.path(),
        &mut router_sessions,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
        BTreeSet::new(),
        3,
        Vec::new(),
    );

    assert_eq!(
        lookup_answer,
        build_expected_refusal(
            "no session named `S-quiet-lake` answered; 3 running sessions did not answer, so \
             their names are unknown"
        )
    );
}

#[test]
fn a_lookup_no_listed_session_answers_is_refused_while_a_path_could_not_be_read() {
    let unlisted_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let name_lookup_answer = advance_lookup_once(
        runtime_directory.path(),
        &mut router_sessions,
        SessionSelector::SessionName("S-quiet-lake".to_string()),
        BTreeSet::new(),
        1,
        vec![build_unread_shared_folder()],
    );
    let id_lookup_answer = advance_lookup_once(
        runtime_directory.path(),
        &mut router_sessions,
        SessionSelector::SessionId(unlisted_session_id),
        BTreeSet::new(),
        0,
        vec![build_unread_shared_folder()],
    );

    assert_eq!(
        name_lookup_answer,
        build_expected_refusal(
            "no session named `S-quiet-lake` answered, and the names of some sessions are \
             unknown (1 running session did not answer; /home/user/koshi-shared/1002 could not \
             be read: Input/output error (os error 5))"
        )
    );
    assert_eq!(
        id_lookup_answer,
        build_expected_refusal(&format!(
            "cannot tell whether session {unlisted_session_id} is running \
             (/home/user/koshi-shared/1002 could not be read: Input/output error (os error 5))"
        ))
    );
}

#[cfg(unix)]
#[test]
fn a_lookup_while_this_users_sessions_cannot_be_listed_is_refused_naming_the_runtime_directory() {
    use std::os::unix::fs::PermissionsExt;

    let unlisted_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());
    std::fs::set_permissions(
        runtime_directory.path(),
        std::fs::Permissions::from_mode(0o300),
    )
    .expect("make the runtime directory unlistable");
    let Err(runtime_read_error) = std::fs::read_dir(runtime_directory.path()) else {
        eprintln!(
            "skipped `a_lookup_while_this_users_sessions_cannot_be_listed_is_refused_naming_the_runtime_directory`: \
             this user lists a mode-300 directory"
        );
        let _ = std::fs::set_permissions(
            runtime_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        );
        return;
    };

    let lookup_answer = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        None,
        SessionSelector::SessionId(unlisted_session_id),
    );

    let _ = std::fs::set_permissions(
        runtime_directory.path(),
        std::fs::Permissions::from_mode(0o700),
    );
    assert_eq!(
        lookup_answer,
        build_expected_refusal(&format!(
            "cannot tell whether session {unlisted_session_id} is running ({})",
            UnreadPath::from_read_error(runtime_directory.path(), &runtime_read_error)
        ))
    );
}

#[cfg(unix)]
#[test]
fn a_name_lookup_while_the_shared_directory_cannot_be_read_is_refused_naming_it() {
    // A link to itself fails every read with `ELOOP`.
    let runtime_directory = build_test_runtime_directory();
    let looping_shared_directory = runtime_directory.path().join("looping");
    std::os::unix::fs::symlink("looping", &looping_shared_directory)
        .expect("link the shared directory to itself");
    let shared_read_error =
        std::fs::read_dir(&looping_shared_directory).expect_err("a link to itself cannot be read");
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let lookup_answer = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        Some(&looping_shared_directory),
        SessionSelector::SessionName("S-quiet-lake".to_string()),
    );

    assert_eq!(
        lookup_answer,
        build_expected_refusal(&format!(
            "no session named `S-quiet-lake` answered, and the names of some sessions are \
             unknown ({})",
            UnreadPath::from_read_error(&looping_shared_directory, &shared_read_error)
        ))
    );
}

/// Plant one socket nothing listens on for `session_id` in each of two
/// folders of `shared_sessions_base_directory`, named after user ids above the
/// owner of `runtime_directory`, and hand back that owner's user id.
#[cfg(unix)]
fn plant_duplicated_foreign_session(
    shared_sessions_base_directory: &Path,
    runtime_directory: &Path,
    session_id: SessionId,
) -> u32 {
    use std::os::unix::fs::MetadataExt;

    let own_user_id = std::fs::metadata(runtime_directory)
        .expect("read the runtime directory")
        .uid();
    for folder_offset in [1, 2] {
        let user_directory =
            shared_sessions_base_directory.join((own_user_id + folder_offset).to_string());
        std::fs::create_dir_all(&user_directory).expect("create a user's folder");
        drop(
            std::os::unix::net::UnixListener::bind(compute_socket_address(
                &user_directory,
                session_id,
            ))
            .expect("plant a socket"),
        );
    }
    own_user_id
}

#[cfg(unix)]
#[test]
fn a_lookup_by_an_id_two_folders_advertise_is_refused_naming_their_owner() {
    let duplicated_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    let own_user_id = plant_duplicated_foreign_session(
        shared_sessions_base_directory.path(),
        runtime_directory.path(),
        duplicated_session_id,
    );
    let mut router_sessions = RouterSessions::from_session_registry(SessionRegistry::new());

    let lookup_answer = run_attach_lookup(
        runtime_directory.path(),
        &mut router_sessions,
        Some(shared_sessions_base_directory.path()),
        SessionSelector::SessionId(duplicated_session_id),
    );

    assert_eq!(
        lookup_answer,
        build_expected_refusal(&format!(
            "session {duplicated_session_id} is advertised 2 times in the shared directory, by \
             user id {own_user_id}; koshi reaches none of them"
        ))
    );
}

#[cfg(unix)]
#[test]
fn a_lookup_the_list_already_answers_reads_no_folder_of_the_shared_directory() {
    // The shared directory holds an id two folders advertise, which any read
    // of it reports. A listed id, and a name a listed session of this user's
    // carries, read nothing.
    let listed_foreign_session_id = SessionId::new();
    let own_session_id = SessionId::new();
    let runtime_directory = build_test_runtime_directory();
    let shared_sessions_base_directory = build_test_runtime_directory();
    plant_duplicated_foreign_session(
        shared_sessions_base_directory.path(),
        runtime_directory.path(),
        listed_foreign_session_id,
    );
    let mut session_registry = build_session_registry(&[(own_session_id, "S-quiet-lake")]);
    session_registry.insert(
        listed_foreign_session_id,
        build_foreign_session_record(listed_foreign_session_id, "S-loud-river"),
    );

    let listings: Vec<ForeignSessionListing> = [
        SessionSelector::SessionId(listed_foreign_session_id),
        SessionSelector::SessionName("S-quiet-lake".to_string()),
    ]
    .iter()
    .map(|session_selector| {
        list_foreign_sessions_for_lookup(
            Some(shared_sessions_base_directory.path()),
            runtime_directory.path(),
            &session_registry,
            session_selector,
        )
        .expect("nothing is read")
    })
    .collect();
    let name_listing = list_foreign_sessions_for_lookup(
        Some(shared_sessions_base_directory.path()),
        runtime_directory.path(),
        &session_registry,
        &SessionSelector::SessionName("S-amber-fox".to_string()),
    )
    .expect("the shared directory is read");

    assert_eq!(
        listings,
        vec![
            ForeignSessionListing::default(),
            ForeignSessionListing::default()
        ]
    );
    assert_eq!(
        name_listing,
        ipc_client::list_foreign_sessions(
            shared_sessions_base_directory.path(),
            runtime_directory.path()
        )
    );
    assert_eq!(name_listing.duplicated_sessions.len(), 1);
}
