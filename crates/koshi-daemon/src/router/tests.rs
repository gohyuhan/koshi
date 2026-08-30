//! Tests for the router's session list, its dispatcher loop, and its remote
//! access.
//!
//! Most run in process against a hand-built list: no real router is bound and
//! no real session server is started, so the name walk, selector resolution,
//! removal, the idle-exit rule, the lock handover, the answer to a restart
//! request, the report a session server prints, and the three remote access
//! token requests are exercised on their own. Starting a real router and a
//! real session server needs whole processes; that is covered by the
//! integration tests instead.
//!
//! Where the piece under test reads something real, a real thing stands in
//! for it: a bound listener for a session the router probes or asks to
//! describe itself, and a `/bin/sh` child for a process the router waits on
//! or kills.
//!
//! The remote access cut goes further than that: it opens the real TLS
//! listener on a loopback port, dials it with the real client, and stands one
//! socket in for the session behind the bridge. So the connection a revoke has
//! to end is a real one, admitted by a real secret.

use super::*;

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use koshi_core::discovery::{SessionInfo, SessionOverview};
use koshi_ipc::endpoint::RESTART_WINDOW;
use koshi_ipc::endpoint::{advert_path, shared_socket_addr};
use koshi_ipc::protocol::{
    IncomingResponse, IpcRequest, IpcResponse, IpcResult, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use koshi_ipc::remote_tokens::{hash_token, TokenEntry, TokenRecord, TOKEN_STORE_FORMAT};
use koshi_ipc::remote_wire::{
    self, RemoteClientFrame, RemoteServerFrame, MIN_REMOTE_PROTOCOL_VERSION,
    REMOTE_PROTOCOL_VERSION, REMOTE_REFUSED,
};
use koshi_ipc::router::RouterRequest;
use koshi_link::remote_client::{self, DIAL_WAIT};
use koshi_test_support::fixtures::test_runtime_dir;

/// One list holding `entries`, each given as its id and its name.
fn registry_of(entries: &[(SessionId, &str)]) -> Registry {
    entries
        .iter()
        .map(|(id, name)| {
            (
                *id,
                SessionEntry {
                    name: (*name).to_string(),
                    socket: socket_addr(Path::new("/nowhere"), *id),
                    pid: 4242,
                },
            )
        })
        .collect()
}

#[test]
fn the_name_walk_rejects_a_name_the_list_already_holds() {
    // The router picks a session's name, so a name already in use must read
    // as taken; the walk moves on only for the names it is told about.
    let taken = SessionId::new();
    let registry = registry_of(&[(taken, "S-quiet-lake")]);

    assert!(name_is_taken(&registry, "S-quiet-lake"));
    assert!(!name_is_taken(&registry, "S-loud-river"));
    assert!(!name_is_taken(&registry, "S-quiet-lak"));
    assert!(!name_is_taken(&registry, "S-quiet-lakes"));
}

#[test]
fn the_name_walk_over_an_empty_list_takes_the_first_name_it_tries() {
    let registry = Registry::new();
    let name = generate_name(NameKind::Session, |candidate| {
        name_is_taken(&registry, candidate)
    });

    assert_eq!(name.split('-').next(), Some("S"));
    assert!(!name_is_taken(&registry, &name));
}

#[test]
fn the_name_walk_hands_back_a_name_the_list_does_not_hold() {
    // With one name taken, a second walk must land somewhere else, so two
    // sessions never share a name.
    let first = SessionId::new();
    let mut registry = Registry::new();
    let taken = generate_name(NameKind::Session, |candidate| {
        name_is_taken(&registry, candidate)
    });
    registry = registry_of(&[(first, &taken)]);

    let second = generate_name(NameKind::Session, |candidate| {
        name_is_taken(&registry, candidate)
    });

    assert_ne!(second, taken);
    assert!(!name_is_taken(&registry, &second));
}

#[test]
fn a_selector_resolves_by_id() {
    let wanted = SessionId::new();
    let other = SessionId::new();
    let absent = SessionId::new();
    let registry = registry_of(&[(wanted, "S-quiet-lake"), (other, "S-loud-river")]);

    assert_eq!(
        resolve(&registry, &SessionSelector::Id(wanted)),
        Some(wanted)
    );
    assert_eq!(resolve(&registry, &SessionSelector::Id(other)), Some(other));
    assert_eq!(resolve(&registry, &SessionSelector::Id(absent)), None);
}

#[test]
fn a_selector_resolves_by_the_whole_name_only() {
    // `S-quiet` is a prefix of `S-quiet-lake` and resolves to nothing.
    let wanted = SessionId::new();
    let registry = registry_of(&[(wanted, "S-quiet-lake"), (SessionId::new(), "S-loud-river")]);

    assert_eq!(
        resolve(
            &registry,
            &SessionSelector::Name("S-quiet-lake".to_string())
        ),
        Some(wanted)
    );
    assert_eq!(
        resolve(&registry, &SessionSelector::Name("S-quiet".to_string())),
        None
    );
    assert_eq!(
        resolve(
            &registry,
            &SessionSelector::Name("s-quiet-lake".to_string())
        ),
        None
    );
    assert_eq!(
        resolve(&registry, &SessionSelector::Name(String::new())),
        None
    );
}

#[test]
fn removing_one_session_leaves_every_other_entry_in_place() {
    let gone = SessionId::new();
    let stays = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let mut registry = registry_of(&[(gone, "S-quiet-lake"), (stays, "S-loud-river")]);

    unregister(runtime_dir.path(), &mut registry, gone);

    assert_eq!(
        registry,
        registry_of(&[(stays, "S-loud-river")]),
        "only the session that exited leaves the list"
    );
}

#[test]
fn removing_a_session_takes_the_files_it_advertised_with_it() {
    // A session server that is gone must stop being discoverable: its
    // endpoint file is what the next router's rebuild walks.
    let gone = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let endpoint_path = EndpointFile::path(runtime_dir.path(), gone);
    EndpointFile {
        socket: socket_addr(runtime_dir.path(), gone),
        token: ConnectionToken::new("a".repeat(64)),
        pid: 4242,
    }
    .write(&endpoint_path)
    .expect("the endpoint file is written");
    #[cfg(unix)]
    let socket_path = PathBuf::from(socket_addr(runtime_dir.path(), gone));
    #[cfg(unix)]
    std::fs::write(&socket_path, b"").expect("the leftover socket file is created");

    let mut registry = registry_of(&[(gone, "S-quiet-lake")]);
    unregister(runtime_dir.path(), &mut registry, gone);

    assert_eq!(registry, Registry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
    #[cfg(unix)]
    assert!(!socket_path.exists(), "the socket file is removed");
}

#[test]
fn removing_a_session_that_is_not_in_the_list_still_clears_its_files() {
    // A session server adopted from an earlier router has files but was
    // dropped from the list by an earlier probe.
    let gone = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let endpoint_path = EndpointFile::path(runtime_dir.path(), gone);
    EndpointFile {
        socket: socket_addr(runtime_dir.path(), gone),
        token: ConnectionToken::new("b".repeat(64)),
        pid: 4242,
    }
    .write(&endpoint_path)
    .expect("the endpoint file is written");

    let mut registry = Registry::new();
    unregister(runtime_dir.path(), &mut registry, gone);

    assert_eq!(registry, Registry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

#[test]
fn the_rebuild_over_an_empty_runtime_directory_finds_no_session() {
    let runtime_dir = test_runtime_dir();

    assert_eq!(sweep(runtime_dir.path(), None), Registry::new());
}

#[test]
fn the_rebuild_drops_an_endpoint_nothing_listens_behind() {
    // The file outlived its session server, so the rebuild must remove it
    // rather than advertise a session no caller can reach.
    let dead = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let endpoint_path = EndpointFile::path(runtime_dir.path(), dead);
    EndpointFile {
        socket: socket_addr(runtime_dir.path(), dead),
        token: ConnectionToken::new("c".repeat(64)),
        pid: 4242,
    }
    .write(&endpoint_path)
    .expect("the endpoint file is written");

    assert_eq!(sweep(runtime_dir.path(), None), Registry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

/// Write a resume file for `session` in `runtime_dir` and stamp it `age` old,
/// the way a session server about to replace its own image leaves one behind.
fn aged_resume_file(runtime_dir: &Path, session: SessionId, age: Duration) -> PathBuf {
    let path = resume_path(runtime_dir, session);
    let file = std::fs::File::create(&path).expect("the resume file is written");
    file.set_modified(SystemTime::now() - age)
        .expect("the resume file is aged");
    path
}

/// Older than the window a swap has to come back in, so the swap that wrote it
/// is dead.
const PAST_THE_WINDOW: Duration = Duration::from_secs(RESTART_WINDOW.as_secs() + 1);

/// Well inside the window a swap has to come back in, so the swap that wrote it
/// may still be in flight.
const INSIDE_THE_WINDOW: Duration = Duration::from_secs(1);

#[test]
fn removing_a_session_takes_its_resume_file_with_it() {
    // A swap that died leaves the file behind holding every pane's screen and
    // scrollback. Nothing else on the machine ever reads it again.
    let gone = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let endpoint_path = EndpointFile::path(runtime_dir.path(), gone);
    EndpointFile {
        socket: socket_addr(runtime_dir.path(), gone),
        token: ConnectionToken::new("d".repeat(64)),
        pid: 4242,
    }
    .write(&endpoint_path)
    .expect("the endpoint file is written");
    let resume_file = aged_resume_file(runtime_dir.path(), gone, PAST_THE_WINDOW);

    let mut registry = registry_of(&[(gone, "S-quiet-lake")]);
    unregister(runtime_dir.path(), &mut registry, gone);

    assert_eq!(registry, Registry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
    assert!(!resume_file.exists(), "the resume file is removed");
}

#[test]
fn removing_a_session_that_is_replacing_its_image_leaves_it_and_its_files_alone() {
    // A resume file younger than the window marks a swap in flight. The
    // session keeps its place in the list and every file it advertised with:
    // its new image rebinds the socket and rewrites the endpoint file.
    let swapping = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let endpoint_path = EndpointFile::path(runtime_dir.path(), swapping);
    EndpointFile {
        socket: socket_addr(runtime_dir.path(), swapping),
        token: ConnectionToken::new("e".repeat(64)),
        pid: 4242,
    }
    .write(&endpoint_path)
    .expect("the endpoint file is written");
    let resume_file = aged_resume_file(runtime_dir.path(), swapping, INSIDE_THE_WINDOW);

    let mut registry = registry_of(&[(swapping, "S-quiet-lake")]);
    unregister(runtime_dir.path(), &mut registry, swapping);

    assert_eq!(
        registry,
        registry_of(&[(swapping, "S-quiet-lake")]),
        "the session stays in the list across the swap"
    );
    assert!(endpoint_path.exists(), "the endpoint file is left in place");
    assert!(resume_file.exists(), "the resume file is left in place");
}

#[test]
fn the_rebuild_removes_a_resume_file_no_session_claims() {
    // A resume file older than the window, with no endpoint file beside it.
    let dead = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let resume_file = aged_resume_file(runtime_dir.path(), dead, PAST_THE_WINDOW);

    assert_eq!(sweep(runtime_dir.path(), None), Registry::new());
    assert!(!resume_file.exists(), "the orphan resume file is removed");
}

#[test]
fn the_rebuild_leaves_a_resume_file_a_swap_is_still_writing_its_way_out_of() {
    // The session server has written the file and has yet to bind its new
    // socket. Removing it here would cost that session every pane's screen.
    let swapping = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let resume_file = aged_resume_file(runtime_dir.path(), swapping, INSIDE_THE_WINDOW);

    assert_eq!(sweep(runtime_dir.path(), None), Registry::new());
    assert!(
        resume_file.exists(),
        "a swap in flight keeps its resume file"
    );
}

#[test]
fn the_rebuild_leaves_the_resume_file_of_a_session_that_is_still_running() {
    // The file is old enough to look dead, and the session it belongs to is in
    // the list. The list is what decides, so nothing of a live session is
    // removed.
    let live = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let resume_file = aged_resume_file(runtime_dir.path(), live, PAST_THE_WINDOW);
    let registry = registry_of(&[(live, "S-quiet-lake")]);

    remove_orphan_resume_files(runtime_dir.path(), &registry);

    assert!(
        resume_file.exists(),
        "a running session keeps its resume file"
    );
}

/// Advertise `session` in `shared_base` the way a session another local user
/// started advertises itself, and hand back the control-socket address that
/// names. On Unix that is a subdirectory named after another user's id; on
/// Windows it is a marker file beside the ones this user writes.
fn advertise_foreign(shared_base: &Path, runtime_dir: &Path, session: SessionId) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let own = std::fs::metadata(runtime_dir)
            .expect("read the runtime directory")
            .uid();
        let user_dir = shared_base.join((own + 1).to_string());
        std::fs::create_dir_all(&user_dir).expect("create the other user's directory");
        shared_socket_addr(&user_dir, session)
    }
    #[cfg(windows)]
    {
        let _ = runtime_dir;
        std::fs::create_dir_all(shared_base).expect("create the shared directory");
        std::fs::write(advert_path(shared_base, session), b"").expect("plant the marker");
        shared_socket_addr(shared_base, session)
    }
}

/// A stand-in koshi another local user started, serving one discovery
/// exchange at `addr`: accept one caller, answer the Hello whatever it
/// presents, and describe a session named `name` created at `created_at`.
fn foreign_session_server(
    addr: &str,
    session: SessionId,
    name: &str,
    created_at: SystemTime,
) -> JoinHandle<()> {
    let listener = Listener::bind(addr).expect("bind the other user's session");
    let overview = SessionOverview {
        session: SessionInfo {
            id: session,
            name: name.to_string(),
            created_at,
            attached_clients: Vec::new(),
            pane_count: 0,
        },
        tabs: Vec::new(),
        panes: Vec::new(),
        clients: Vec::new(),
    };
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the router");
        let hello: IpcRequest = connection.recv().expect("read hello");
        let query: IpcRequest = connection.recv().expect("read discovery request");
        let replies = [
            IpcResponse {
                request_id: Some(hello.request_id),
                result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
            },
            IpcResponse {
                request_id: Some(query.request_id),
                result: IpcResult::Overview(overview),
            },
        ];
        for reply in replies {
            connection.send(&reply).expect("send the scripted reply");
        }
    })
}

#[test]
fn the_rebuild_registers_a_session_another_local_user_started() {
    // Only visibility crosses users: the router lists that session and hands
    // out its address, and names no process of its own for it.
    let runtime_dir = test_runtime_dir();
    let shared = test_runtime_dir();
    let session = SessionId::new();
    let created_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let addr = advertise_foreign(shared.path(), runtime_dir.path(), session);
    let server = foreign_session_server(&addr, session, "S-quiet-lake", created_at);

    let registry = sweep(runtime_dir.path(), Some(shared.path()));

    assert_eq!(
        registry,
        Registry::from([(
            session,
            SessionEntry {
                name: "S-quiet-lake".to_string(),
                socket: addr,
                pid: 0,
            },
        )]),
    );

    server.join().expect("the other user's session exits");
}

#[test]
fn the_rebuild_leaves_out_a_shared_advert_nothing_listens_behind() {
    // The other user's session crashed and left its advert behind. The
    // rebuild must skip it and remove nothing: those files are that user's.
    let runtime_dir = test_runtime_dir();
    let shared = test_runtime_dir();
    let session = SessionId::new();
    let addr = advertise_foreign(shared.path(), runtime_dir.path(), session);
    // On Unix the socket file the session bound outlives it; on Windows the
    // pipe went with the process, so only the marker is left.
    let leftover = if cfg!(unix) {
        std::fs::write(&addr, b"").expect("plant the leftover socket file");
        PathBuf::from(&addr)
    } else {
        advert_path(shared.path(), session)
    };

    assert_eq!(
        sweep(runtime_dir.path(), Some(shared.path())),
        Registry::new()
    );
    assert!(leftover.exists(), "the other user's advert is left alone");
}

/// A short idle window, so an idle-exit test finishes quickly.
const TEST_IDLE_EXIT: Duration = Duration::from_millis(50);

/// The path a test hands the loop as the binary a restart would start. No
/// test here starts it.
fn test_exe() -> PathBuf {
    std::env::current_exe().expect("this test binary's own path")
}

/// Remote access as a machine that has none holds it: no listen address, no
/// data directory, no listener, and nothing carried. No test here opens a
/// remote connection.
fn no_remote() -> RemoteState {
    RemoteState {
        address: None,
        data_dir: None,
        listening: false,
        live: Vec::new(),
        next_id: 0,
        said_full: Occasional::new(),
    }
}

#[test]
fn an_idle_window_that_passes_with_no_session_running_ends_the_loop() {
    let runtime_dir = test_runtime_dir();
    let (events_tx, events_rx) = mpsc::channel();
    let mut registry = Registry::new();

    let exit = dispatch(
        runtime_dir.path(),
        &test_exe(),
        None,
        &events_tx,
        &events_rx,
        TEST_IDLE_EXIT,
        &mut registry,
        &mut no_remote(),
    );

    assert_eq!(exit, RouterExit::Idle);
    assert_eq!(registry, Registry::new());
}

#[test]
fn a_request_inside_the_idle_window_is_served_and_the_loop_goes_on() {
    // A create arriving just as the router would have exited must still be
    // answered, so a caller's first request is never dropped on the floor.
    let runtime_dir = test_runtime_dir();
    let (events_tx, events_rx) = mpsc::channel();
    let (reply, answer) = mpsc::channel();
    let mut registry = Registry::new();
    events_tx
        .send(RouterEvent::Request {
            kind: RouterRequestKind::ListSessions,
            reply,
        })
        .expect("the request is queued");

    let exit = dispatch(
        runtime_dir.path(),
        &test_exe(),
        None,
        &events_tx,
        &events_rx,
        TEST_IDLE_EXIT,
        &mut registry,
        &mut no_remote(),
    );

    assert_eq!(
        answer.try_recv().expect("the loop answered the request"),
        RouterResult::Sessions(Vec::new())
    );
    assert_eq!(exit, RouterExit::Idle);
    assert_eq!(registry, Registry::new());
}

#[test]
fn a_delivered_restart_reply_ends_the_loop_for_the_swap() {
    // The reply is written before the restart, so the loop ends only once the
    // connection thread reports the write. The list is left as it stood: the
    // sessions outlive the restart.
    let running = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let (events_tx, events_rx) = mpsc::channel();
    let mut registry = registry_of(&[(running, "S-quiet-lake")]);
    events_tx
        .send(RouterEvent::RestartDelivered)
        .expect("the delivered reply is queued");

    let exit = dispatch(
        runtime_dir.path(),
        &test_exe(),
        None,
        &events_tx,
        &events_rx,
        TEST_IDLE_EXIT,
        &mut registry,
        &mut no_remote(),
    );

    assert_eq!(exit, RouterExit::Restart);
    assert_eq!(registry, registry_of(&[(running, "S-quiet-lake")]));
}

#[test]
fn a_running_session_keeps_the_loop_alive_past_the_idle_window() {
    // The idle window is read off the list, not off the last request: a
    // router holding a session must wait for that session however long it
    // runs.
    let running = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let (events_tx, events_rx) = mpsc::channel();
    let sender = events_tx.clone();
    let held = runtime_dir.path().to_path_buf();

    let loop_thread = std::thread::spawn(move || {
        let mut registry = registry_of(&[(running, "S-quiet-lake")]);
        let exit = dispatch(
            &held,
            &test_exe(),
            None,
            &events_tx,
            &events_rx,
            TEST_IDLE_EXIT,
            &mut registry,
            &mut no_remote(),
        );
        (exit, registry)
    });

    std::thread::sleep(TEST_IDLE_EXIT * 5);
    assert!(
        !loop_thread.is_finished(),
        "the loop is still serving while a session is running"
    );

    sender
        .send(RouterEvent::ChildExited(running))
        .expect("the exit is queued");
    let (exit, left) = loop_thread.join().expect("the loop ended");

    assert_eq!(exit, RouterExit::Idle);
    assert_eq!(
        left,
        Registry::new(),
        "the session that exited left the list, and the empty list ended the loop"
    );
}

#[test]
fn a_session_that_exits_while_another_runs_leaves_the_loop_serving() {
    let gone = SessionId::new();
    let stays = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let (events_tx, events_rx) = mpsc::channel();
    let sender = events_tx.clone();
    let held = runtime_dir.path().to_path_buf();

    let loop_thread = std::thread::spawn(move || {
        let mut registry = registry_of(&[(gone, "S-quiet-lake"), (stays, "S-loud-river")]);
        let exit = dispatch(
            &held,
            &test_exe(),
            None,
            &events_tx,
            &events_rx,
            TEST_IDLE_EXIT,
            &mut registry,
            &mut no_remote(),
        );
        (exit, registry)
    });

    sender
        .send(RouterEvent::ChildExited(gone))
        .expect("the exit is queued");
    std::thread::sleep(TEST_IDLE_EXIT * 5);
    assert!(
        !loop_thread.is_finished(),
        "one session left, so the loop keeps serving"
    );

    sender
        .send(RouterEvent::ChildExited(stays))
        .expect("the second exit is queued");
    let (exit, left) = loop_thread.join().expect("the loop ended");

    assert_eq!(exit, RouterExit::Idle);
    assert_eq!(left, Registry::new());
}

#[test]
fn a_lookup_for_a_session_the_list_does_not_hold_is_refused_by_name() {
    let runtime_dir = test_runtime_dir();
    let mut registry = Registry::new();

    let answer = attach_lookup(
        runtime_dir.path(),
        &mut registry,
        &SessionSelector::Name("S-quiet-lake".to_string()),
    );

    assert_eq!(
        answer,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: "no session named `S-quiet-lake` is running".to_string(),
        })
    );
}

#[test]
fn a_lookup_finding_nothing_listening_drops_the_session_and_its_files() {
    // This is how a session server that outlived an earlier router is
    // noticed: nobody is its parent, so only the probe reports it gone.
    let dead = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let endpoint_path = EndpointFile::path(runtime_dir.path(), dead);
    EndpointFile {
        socket: socket_addr(runtime_dir.path(), dead),
        token: ConnectionToken::new("d".repeat(64)),
        pid: 4242,
    }
    .write(&endpoint_path)
    .expect("the endpoint file is written");
    let mut registry = registry_of(&[(dead, "S-quiet-lake")]);
    registry
        .get_mut(&dead)
        .expect("the session is listed")
        .socket = socket_addr(runtime_dir.path(), dead);

    let answer = attach_lookup(
        runtime_dir.path(),
        &mut registry,
        &SessionSelector::Id(dead),
    );

    assert_eq!(
        answer,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::NotFound,
            message: format!("no session {dead} is running"),
        })
    );
    assert_eq!(registry, Registry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

/// A stand-in session server at `addr` that is bound and answering, and
/// settles the Hello on `PROTOCOL_VERSION + 1` — a version outside the range
/// this build asks for, which fails the exchange without the session being
/// gone.
fn version_mismatched_session_server(addr: &str) -> JoinHandle<()> {
    let listener = Listener::bind(addr).expect("bind the live session");
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the router");
        let hello: IpcRequest = connection.recv().expect("read hello");
        let _query: IpcRequest = connection.recv().expect("read discovery request");
        let _ = connection.send(&IpcResponse {
            request_id: Some(hello.request_id),
            result: IpcResult::Hello {
                protocol_version: PROTOCOL_VERSION + 1,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        });
    })
}

#[test]
fn a_listing_keeps_a_session_that_answers_with_a_version_this_build_does_not_read() {
    let live = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let socket = socket_addr(runtime_dir.path(), live);
    let endpoint_path = EndpointFile::path(runtime_dir.path(), live);
    EndpointFile {
        socket: socket.clone(),
        token: ConnectionToken::new("e".repeat(64)),
        pid: 4242,
    }
    .write(&endpoint_path)
    .expect("the endpoint file is written");
    let server = version_mismatched_session_server(&socket);

    let mut registry = registry_of(&[(live, "S-quiet-lake")]);
    registry
        .get_mut(&live)
        .expect("the session is listed")
        .socket
        .clone_from(&socket);

    let answer = list_sessions(runtime_dir.path(), &mut registry);
    server.join().expect("the stand-in session ended");

    assert_eq!(
        answer,
        RouterResult::Sessions(Vec::new()),
        "a session that could not describe itself is left out of the answer"
    );
    assert!(
        registry.contains_key(&live),
        "a session that is still bound stays in the list"
    );
    assert!(
        endpoint_path.exists(),
        "a session that is still bound keeps its endpoint file"
    );
}

#[test]
fn the_rebuild_keeps_the_files_of_a_session_it_cannot_read_a_version_from() {
    let live = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let socket = socket_addr(runtime_dir.path(), live);
    let endpoint_path = EndpointFile::path(runtime_dir.path(), live);
    EndpointFile {
        socket: socket.clone(),
        token: ConnectionToken::new("f".repeat(64)),
        pid: 4242,
    }
    .write(&endpoint_path)
    .expect("the endpoint file is written");
    let server = version_mismatched_session_server(&socket);

    let registry = sweep(runtime_dir.path(), None);
    server.join().expect("the stand-in session ended");

    assert_eq!(
        registry,
        Registry::new(),
        "a session that could not describe itself is not listed"
    );
    assert!(
        endpoint_path.exists(),
        "a session that is still bound keeps its endpoint file"
    );
}

/// A stand-in session server at `addr` that accepts one connection and ends.
/// A lookup's probe sends nothing and closes, so that is all it needs.
fn probe_answering_server(addr: &str) -> JoinHandle<()> {
    let listener = Listener::bind(addr).expect("bind the stand-in session");
    std::thread::spawn(move || {
        let _ = listener.accept();
    })
}

#[test]
fn a_lookup_for_a_session_that_answers_hands_back_where_it_listens() {
    // The lookup probes the address before it hands it out. A session that
    // accepts the connection is answered with the name, address and process
    // id the list holds for it.
    let live = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let socket = socket_addr(runtime_dir.path(), live);
    let server = probe_answering_server(&socket);
    let mut registry = registry_of(&[(live, "S-quiet-lake")]);
    registry
        .get_mut(&live)
        .expect("the session is listed")
        .socket
        .clone_from(&socket);

    let answer = attach_lookup(
        runtime_dir.path(),
        &mut registry,
        &SessionSelector::Name("S-quiet-lake".to_string()),
    );

    assert_eq!(
        answer,
        RouterResult::Found(SessionAddress {
            id: live,
            name: "S-quiet-lake".to_string(),
            socket: socket.clone(),
            pid: 4242,
        })
    );
    assert_eq!(
        registry,
        Registry::from([(
            live,
            SessionEntry {
                name: "S-quiet-lake".to_string(),
                socket,
                pid: 4242,
            },
        )]),
        "a session that answered stays in the list"
    );

    server.join().expect("the stand-in session ended");
}

#[test]
fn a_listing_answers_the_sessions_that_describe_themselves_in_name_then_id_order() {
    // The list is a map, so its own order is not the answer's. The answer is
    // sorted by name, then by id.
    let runtime_dir = test_runtime_dir();
    let loud = SessionId::new();
    let quiet = SessionId::new();
    let created_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let mut servers = Vec::new();
    for (id, name) in [(loud, "S-loud-river"), (quiet, "S-quiet-lake")] {
        let socket = socket_addr(runtime_dir.path(), id);
        servers.push(foreign_session_server(&socket, id, name, created_at));
        EndpointFile {
            socket,
            token: ConnectionToken::new("f".repeat(64)),
            pid: 4242,
        }
        .write(&EndpointFile::path(runtime_dir.path(), id))
        .expect("the endpoint file is written");
    }
    let mut registry = registry_of(&[(quiet, "S-quiet-lake"), (loud, "S-loud-river")]);

    let answer = list_sessions(runtime_dir.path(), &mut registry);

    assert_eq!(
        answer,
        RouterResult::Sessions(vec![
            SessionInfo {
                id: loud,
                name: "S-loud-river".to_string(),
                created_at,
                attached_clients: Vec::new(),
                pane_count: 0,
            },
            SessionInfo {
                id: quiet,
                name: "S-quiet-lake".to_string(),
                created_at,
                attached_clients: Vec::new(),
                pane_count: 0,
            },
        ])
    );
    assert_eq!(
        registry,
        registry_of(&[(quiet, "S-quiet-lake"), (loud, "S-loud-river")]),
        "both sessions answered, so neither left the list"
    );

    for server in servers {
        server.join().expect("the stand-in session ended");
    }
}

#[test]
fn a_listing_drops_every_session_that_does_not_answer() {
    let dead = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let mut registry = registry_of(&[(dead, "S-quiet-lake")]);

    let answer = list_sessions(runtime_dir.path(), &mut registry);

    assert_eq!(answer, RouterResult::Sessions(Vec::new()));
    assert_eq!(registry, Registry::new());
}

#[test]
fn the_session_server_starts_in_the_directory_the_request_named() {
    // The first shell inherits the session server's directory, so the caller's
    // directory reaches the shell only if it is set on the child here.
    let runtime_dir = test_runtime_dir();
    let dir = test_runtime_dir();

    let command = session_server_command(
        runtime_dir.path(),
        SessionId::new(),
        "S-quiet-lake",
        None,
        Some(dir.path()),
        None,
    )
    .expect("the command is built");

    assert_eq!(command.get_current_dir(), Some(dir.path()));
}

/// The arguments a session server is started with, in order, as plain strings.
fn args_of(command: &std::process::Command) -> Vec<String> {
    command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn a_create_that_asked_for_no_other_users_starts_the_session_without_the_flag() {
    let runtime_dir = test_runtime_dir();
    let id = SessionId::new();

    let command = session_server_command(runtime_dir.path(), id, "S-quiet-lake", None, None, None)
        .expect("the command is built");

    assert_eq!(
        args_of(&command),
        vec![
            "serve-session".to_string(),
            id.to_string(),
            "S-quiet-lake".to_string(),
            "--runtime-dir".to_string(),
            runtime_dir.path().to_string_lossy().into_owned(),
        ]
    );
}

#[test]
fn a_create_that_asked_for_the_other_users_starts_the_session_under_the_flag() {
    // The flag is the only thing that carries the answer to the child, so a
    // create asking for the other users and one leaving it to the file differ
    // by exactly this argument.
    let runtime_dir = test_runtime_dir();
    let id = SessionId::new();

    let command = session_server_command(
        runtime_dir.path(),
        id,
        "S-quiet-lake",
        Some("dev"),
        None,
        Some(true),
    )
    .expect("the command is built");

    assert_eq!(
        args_of(&command),
        vec![
            "serve-session".to_string(),
            id.to_string(),
            "S-quiet-lake".to_string(),
            "--runtime-dir".to_string(),
            runtime_dir.path().to_string_lossy().into_owned(),
            "--profile".to_string(),
            "dev".to_string(),
            "--allow-other-users".to_string(),
        ]
    );
}

#[test]
fn a_create_that_refused_the_other_users_starts_the_session_without_the_flag() {
    // `Some(false)` is not a force, so the session's own `koshi.kdl` answers,
    // exactly as it does when the create named nothing.
    let runtime_dir = test_runtime_dir();
    let id = SessionId::new();

    let command = session_server_command(
        runtime_dir.path(),
        id,
        "S-quiet-lake",
        None,
        None,
        Some(false),
    )
    .expect("the command is built");

    assert_eq!(
        args_of(&command),
        vec![
            "serve-session".to_string(),
            id.to_string(),
            "S-quiet-lake".to_string(),
            "--runtime-dir".to_string(),
            runtime_dir.path().to_string_lossy().into_owned(),
        ]
    );
}

/// The router owns no console, so a console child of it would be given a new
/// console, and Windows 11 draws that in a terminal window. `CREATE_NO_WINDOW`
/// is what keeps the session server's console off the screen, and a transposed
/// digit in it is a different flag that brings the window back.
///
/// `std::process::Command` reports no creation flags, so the flag reaching the
/// child is checked by hand on Windows. The value itself is checked here.
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
    let runtime_dir = test_runtime_dir();

    let command = session_server_command(
        runtime_dir.path(),
        SessionId::new(),
        "S-quiet-lake",
        None,
        None,
        None,
    )
    .expect("the command is built");

    assert_eq!(command.get_current_dir(), None);
}

/// How long a lock-handover test holds the lock before releasing it. Well
/// inside [`LOCK_HANDOVER_WAIT`], so the waiting side takes it on a poll
/// rather than on the timeout.
const TEST_LOCK_HOLD: Duration = Duration::from_millis(200);

/// One handle on the router lock file in `runtime_dir`, opened the way
/// [`run_router`] opens it.
fn lock_handle(runtime_dir: &Path) -> File {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(router_lock_path(runtime_dir))
        .expect("the router lock file opens")
}

#[test]
fn a_router_that_does_not_wait_yields_to_the_router_holding_the_lock() {
    let runtime_dir = test_runtime_dir();
    let holder = lock_handle(runtime_dir.path());
    let arriving = lock_handle(runtime_dir.path());

    assert!(
        take_lock(&holder, false).expect("the first router takes the lock"),
        "an unlocked router lock is taken on the first attempt"
    );
    assert!(
        !take_lock(&arriving, false).expect("the second router reads the lock"),
        "a held lock sends the arriving router to the one holding it"
    );
}

#[test]
fn a_router_that_waits_takes_the_lock_the_previous_router_releases() {
    // This is the Windows handover: the replacement router is started while
    // the previous one still holds the lock, and takes it when that router
    // drops it as the last step of its shutdown.
    let runtime_dir = test_runtime_dir();
    let previous = lock_handle(runtime_dir.path());
    let replacement = lock_handle(runtime_dir.path());
    assert!(take_lock(&previous, false).expect("the previous router takes the lock"));

    let shutdown = std::thread::spawn(move || {
        std::thread::sleep(TEST_LOCK_HOLD);
        drop(previous);
    });
    let taken = take_lock(&replacement, true).expect("the replacement waits for the lock");
    shutdown.join().expect("the previous router shut down");

    assert!(taken, "the replacement takes the lock that was released");
}

#[test]
fn a_restart_request_is_answered_from_the_binary_on_disk() {
    let runtime_dir = test_runtime_dir();
    let exe = runtime_dir.path().join("koshi");
    std::fs::write(&exe, b"").expect("the stand-in binary is written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in binary is executable");
    }
    let (events_tx, _events_rx) = mpsc::channel();
    let mut registry = Registry::new();

    let answer = serve_request(
        runtime_dir.path(),
        &exe,
        None,
        &mut registry,
        &mut no_remote(),
        &events_tx,
        RouterRequestKind::Restart,
    );

    assert_eq!(answer, RouterResult::Restarting);
}

#[test]
fn a_restart_request_naming_a_binary_that_cannot_be_read_is_refused() {
    // The reply is the router's only chance to refuse: after it, the restart
    // runs. A path with nothing at it must not reach the restart.
    let runtime_dir = test_runtime_dir();
    let exe = runtime_dir.path().join("koshi");
    let error = std::fs::metadata(&exe).expect_err("nothing is at that path");
    let (events_tx, _events_rx) = mpsc::channel();
    let mut registry = Registry::new();

    let answer = serve_request(
        runtime_dir.path(),
        &exe,
        None,
        &mut registry,
        &mut no_remote(),
        &events_tx,
        RouterRequestKind::Restart,
    );

    assert_eq!(
        answer,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!("the binary at {} could not be read: {error}", exe.display()),
        })
    );
}

// The pre-flight check refuses a binary the kernel would refuse to exec, so
// the router never tears down its dispatch loop for a swap that cannot start.
#[cfg(unix)]
#[test]
fn a_restart_request_naming_a_non_executable_binary_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let runtime_dir = test_runtime_dir();
    let exe = runtime_dir.path().join("koshi");
    std::fs::write(&exe, b"").expect("the stand-in binary is written");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644))
        .expect("the execute permission is dropped");
    let (events_tx, _events_rx) = mpsc::channel();
    let mut registry = Registry::new();

    let answer = serve_request(
        runtime_dir.path(),
        &exe,
        None,
        &mut registry,
        &mut no_remote(),
        &events_tx,
        RouterRequestKind::Restart,
    );

    assert_eq!(
        answer,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!("the binary at {} is not executable", exe.display()),
        })
    );
}

/// A process this test is the parent of, running `script` under `/bin/sh` with
/// its three standard streams going nowhere. Dropping the handle waits on
/// nothing; the caller collects the exit.
#[cfg(unix)]
fn child_running(script: &str) -> Child {
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the shell runs")
}

/// A process this test is the parent of, which ends at once.
#[cfg(unix)]
fn short_lived_child() -> Child {
    child_running("exit 0")
}

/// After a restart in place, the sessions the previous image started are still
/// children of this process, and this is how their exits reach the list.
///
/// The watcher thread holds the only other sender, so an event or a closed
/// channel ends the wait here: nothing waits on a clock.
#[cfg(unix)]
#[test]
fn the_watcher_reports_the_exit_of_a_session_this_process_is_the_parent_of() {
    let id = SessionId::new();
    let (events_tx, events_rx) = mpsc::channel();

    watch_session_exit(short_lived_child().id(), id, events_tx);

    match events_rx.recv() {
        Ok(RouterEvent::ChildExited(reported)) => assert_eq!(reported, id),
        Ok(_) => panic!("the watcher reported something other than the session's exit"),
        Err(mpsc::RecvError) => panic!("the watcher ended without reporting the exit"),
    }
}

/// The reaper waits on the session server the router started and reports its
/// exit, which is what takes that session out of the list.
///
/// The reaper thread holds the only other sender, so an event or a closed
/// channel ends the wait here: nothing waits on a clock.
#[cfg(unix)]
#[test]
fn the_reaper_reports_the_exit_of_the_session_server_it_started() {
    let id = SessionId::new();
    let (events_tx, events_rx) = mpsc::channel();

    start_reaper_thread(short_lived_child(), id, events_tx);

    match events_rx.recv() {
        Ok(RouterEvent::ChildExited(reported)) => assert_eq!(reported, id),
        Ok(_) => panic!("the reaper reported something other than the session's exit"),
        Err(mpsc::RecvError) => panic!("the reaper ended without reporting the exit"),
    }
}

/// A create that never got its ready report kills the child and waits on it.
/// The process ends and its status is collected, so nothing is left running
/// and nothing is left unreaped.
#[cfg(unix)]
#[test]
fn a_child_that_never_became_a_session_is_killed_and_collected() {
    use std::os::unix::process::ExitStatusExt as _;

    let mut child = child_running("sleep 30");

    kill_child(&mut child);

    let status = child
        .try_wait()
        .expect("the child's status reads back")
        .expect("the child was collected, so its status is known");
    assert_eq!(
        status.signal(),
        Some(libc::SIGKILL),
        "the child was killed rather than left running"
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
    let not_a_child = u32::try_from(unsafe { libc::getppid() }).expect("a process id is positive");
    let (events_tx, events_rx) = mpsc::channel();

    watch_session_exit(not_a_child, SessionId::new(), events_tx);

    assert_eq!(events_rx.recv().err(), Some(mpsc::RecvError));
}

/// The router hands its place over on Windows by starting the new binary with
/// these two creation flags and this argument. `std::process::Command` reports
/// neither back, so the values are checked here; the argument is the one
/// [`crate::cli`] parses into `wait_for_lock`.
#[cfg(windows)]
#[test]
fn the_handover_carries_the_win32_flags_and_the_argument_that_waits() {
    assert_eq!(
        DETACHED_PROCESS, 0x0000_0008,
        "DETACHED_PROCESS is 8; another value is another flag"
    );
    assert_eq!(
        CREATE_NEW_PROCESS_GROUP, 0x0000_0200,
        "CREATE_NEW_PROCESS_GROUP is 512; another value is another flag"
    );
    assert_eq!(WAIT_FOR_LOCK_FLAG, "--wait-for-lock");
}

/// A restart that cannot exec must leave the router able to write to a client
/// that hung up. `exec` resets SIGPIPE to `SIG_DFL` before it calls `execvp`,
/// so without the restore that write would end the process instead of
/// returning an error.
///
/// The file the restart names is readable but not executable, so `fs::metadata`
/// succeeds and `execvp` fails with `EACCES`. Reading the disposition installs
/// the same one it reads, so the process is left as the assertion found it.
#[cfg(unix)]
#[test]
fn a_restart_that_cannot_exec_leaves_the_write_to_a_hung_up_client_ignored() {
    use std::os::unix::fs::PermissionsExt;

    let runtime_dir = test_runtime_dir();
    let exe = runtime_dir.path().join("koshi");
    std::fs::write(&exe, b"").expect("the stand-in binary is written");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o644))
        .expect("the stand-in binary is readable and not executable");

    let error = restart_by_exec(&exe, runtime_dir.path());

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    let prior = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    assert_eq!(prior, libc::SIG_IGN);
}

/// The accept loop gates every connection on the user the OS reports for it,
/// so a connection this router's own user opens must still be served. This
/// test's caller runs in the test process, under that same user; another
/// user's connection needs a second OS account and is covered by neither this
/// test nor any other.
#[test]
fn the_accept_loop_serves_a_connection_this_user_opened() {
    let runtime_dir = test_runtime_dir();
    let addr = router_socket_addr(runtime_dir.path());
    let listener = Listener::bind(&addr).expect("the router socket is bound");
    let token = ConnectionToken::generate();
    let (events_tx, events_rx) = mpsc::channel();
    let shutting_down = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutting_down);
    let accepted_token = token.clone();
    let accepting = std::thread::spawn(move || {
        accept_loop(&listener, &accepted_token, &events_tx, &flag);
    });

    let mut caller = Connection::connect(&addr).expect("the caller reaches the router");
    caller
        .send(&RouterRequest {
            request_id: 1,
            kind: RouterRequestKind::hello(token),
        })
        .expect("the hello is written");
    let answered: RouterResponse = caller.recv().expect("the hello is answered");
    assert_eq!(
        answered,
        RouterResponse {
            request_id: Some(1),
            result: RouterResult::Hello {
                protocol_version: ROUTER_PROTOCOL_VERSION,
                version: BUILD_VERSION.to_string(),
            },
        }
    );
    caller
        .send(&RouterRequest {
            request_id: 2,
            kind: RouterRequestKind::ListSessions,
        })
        .expect("the listing request is written");
    let reached = events_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the request reaches the dispatcher");
    let RouterEvent::Request { kind, reply: _ } = reached else {
        panic!("the accept loop passed on something other than a request");
    };
    assert_eq!(kind, RouterRequestKind::ListSessions);

    shutting_down.store(true, Ordering::SeqCst);
    drop(caller);
    let _ = Connection::connect(&addr);
    accepting.join().expect("the accept loop ends");
}

/// One token request answered the way the dispatcher answers it: against the
/// store at `store`, with an empty session list and an events channel nothing
/// reads. `store` is `None` for a machine with no data directory. The runtime
/// directory is a fresh temporary one, which no token request reads.
fn answer_token_request(store: Option<&Path>, kind: RouterRequestKind) -> RouterResult {
    let runtime_dir = test_runtime_dir();
    let (events_tx, _events_rx) = mpsc::channel();
    let mut registry = Registry::new();
    serve_request(
        runtime_dir.path(),
        &test_exe(),
        store,
        &mut registry,
        &mut no_remote(),
        &events_tx,
        kind,
    )
}

/// A grant request for `identity` on `scope`, working for `expires_in` and
/// never stopping on its own when that is `None`.
fn grant_request(
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

/// Make one grant that never stops on its own and hand back its secret. A
/// refused grant fails the calling test.
fn granted_token(store: &Path, identity: &str, scope: TokenScope) -> ConnectionToken {
    let answer = answer_token_request(Some(store), grant_request(identity, scope, None));
    match answer {
        RouterResult::Granted { token, .. } => token,
        other => panic!("the grant was refused: {other:?}"),
    }
}

/// Rewrite the store at `store` with line breaks and indents, and hand back
/// the bytes now on disk.
///
/// The reader takes those bytes and the writer never produces them, so a
/// later byte comparison against them fails if anything wrote the store, even
/// a write that put the same records back.
fn spaced_out(store: &Path) -> Vec<u8> {
    let held = TokenStore::read(store).expect("the store reads back");
    let spaced = serde_json::to_vec_pretty(&held).expect("the store encodes with indents");
    std::fs::write(store, &spaced).expect("the spaced store is written");
    spaced
}

/// The one refusal every token request gets when the store cannot be opened.
fn token_refusal(message: &str) -> RouterResult {
    RouterResult::Error(IpcErrorPayload {
        code: IpcErrorCode::MalformedRequest,
        message: message.to_string(),
    })
}

/// One of each token request, so a test can check that a store which cannot
/// be opened refuses all three the same way.
fn every_token_request(session: SessionId) -> [RouterRequestKind; 3] {
    [
        grant_request("ada", TokenScope::HostWide, None),
        RouterRequestKind::RevokeToken {
            identity: "ada".to_string(),
            scope: None,
        },
        RouterRequestKind::ListTokens {
            scope: Some(TokenScope::Session(session)),
        },
    ]
}

#[test]
fn a_grant_writes_one_record_holding_the_hash_of_the_secret_it_hands_back() {
    // The operator sees the secret once, from the answer. The store keeps only
    // its hash, so a reader of the file cannot open a connection.
    let home = test_runtime_dir();
    let store = store_path(home.path());

    let answer = answer_token_request(
        Some(&store),
        grant_request("ada", TokenScope::HostWide, None),
    );

    let RouterResult::Granted { token, replaced } = answer else {
        panic!("the grant was refused: {answer:?}")
    };
    assert!(!replaced, "the store held no grant for ada to replace");
    let written = TokenStore::read(&store).expect("the store reads back");
    assert_eq!(written.format, TOKEN_STORE_FORMAT);
    assert_eq!(written.records.len(), 1);
    assert_eq!(written.records[0].identity, "ada");
    assert_eq!(written.records[0].hash, hash_token(&token));
    assert_eq!(written.records[0].scope, TokenScope::HostWide);
    assert_eq!(written.records[0].expires_at, None);
    assert_eq!(written.records[0].last_used_at, None);
    assert_eq!(written.records[0].revoked_at, None);
    let bytes = std::fs::read(&store).expect("the store file is on disk");
    assert!(
        !String::from_utf8_lossy(&bytes).contains(token.expose()),
        "the secret itself never reaches the disk"
    );
}

#[test]
fn a_second_grant_replaces_the_one_on_the_same_scope_and_adds_one_on_another() {
    // An identity holds at most one grant per scope, so re-granting the same
    // scope stops the old secret while a second scope stands beside the first.
    let home = test_runtime_dir();
    let store = store_path(home.path());
    let session = SessionId::new();
    let first = granted_token(&store, "ada", TokenScope::HostWide);

    let again = answer_token_request(
        Some(&store),
        grant_request("ada", TokenScope::HostWide, None),
    );

    let RouterResult::Granted {
        token: second,
        replaced,
    } = again
    else {
        panic!("the second grant was refused: {again:?}")
    };
    assert!(replaced, "ada already held a host-wide grant");
    let written = TokenStore::read(&store).expect("the store reads back");
    assert_eq!(written.records.len(), 1);
    assert_eq!(written.records[0].hash, hash_token(&second));
    assert_ne!(
        hash_token(&second),
        hash_token(&first),
        "the replacement hands out a different secret"
    );

    let other_scope = answer_token_request(
        Some(&store),
        grant_request("ada", TokenScope::Session(session), None),
    );

    let RouterResult::Granted { replaced, .. } = other_scope else {
        panic!("the grant on the session scope was refused: {other_scope:?}")
    };
    assert!(!replaced, "ada held no grant on that session");
    let written = TokenStore::read(&store).expect("the store reads back");
    assert_eq!(written.records.len(), 2);
    assert_eq!(written.records[0].scope, TokenScope::HostWide);
    assert_eq!(written.records[1].scope, TokenScope::Session(session));
}

#[test]
fn a_grant_expires_the_given_span_after_the_clock_reading_it_was_issued_at() {
    // The router reads the clock once and stamps both times from that one
    // reading, so the gap between them is exactly the span asked for.
    const A_DAY: Duration = Duration::from_secs(24 * 60 * 60);
    let home = test_runtime_dir();
    let store = store_path(home.path());

    let answer = answer_token_request(
        Some(&store),
        grant_request("ada", TokenScope::HostWide, Some(A_DAY)),
    );

    let RouterResult::Granted { replaced, .. } = answer else {
        panic!("the grant was refused: {answer:?}")
    };
    assert!(!replaced, "the store held no grant for ada to replace");
    let written = TokenStore::read(&store).expect("the store reads back");
    assert_eq!(written.records.len(), 1);
    let expires_at = written.records[0]
        .expires_at
        .expect("the grant carries an expiry");
    assert_eq!(
        expires_at
            .duration_since(written.records[0].issued_at)
            .expect("the expiry is after the issue time"),
        A_DAY
    );

    let no_expiry = answer_token_request(
        Some(&store),
        grant_request("grace", TokenScope::HostWide, None),
    );

    let RouterResult::Granted { replaced, .. } = no_expiry else {
        panic!("the grant was refused: {no_expiry:?}")
    };
    assert!(!replaced, "the store held no grant for grace to replace");
    let written = TokenStore::read(&store).expect("the store reads back");
    assert_eq!(written.records.len(), 2);
    assert_eq!(
        written.records[1].expires_at, None,
        "a grant with no span never stops on its own"
    );
}

#[test]
fn a_span_the_clock_cannot_represent_is_refused_and_leaves_the_store_alone() {
    // The add is checked, so the far-off expiry comes back as a refusal rather
    // than ending the router's own thread. The refusal returns before any
    // write, so the file on disk is untouched either way.
    let home = test_runtime_dir();
    let store = store_path(home.path());
    let too_far = grant_request(
        "ada",
        TokenScope::HostWide,
        Some(Duration::from_secs(u64::MAX)),
    );
    let refusal =
        token_refusal("the expiry is further ahead than this machine's clock can represent");

    assert_eq!(answer_token_request(Some(&store), too_far), refusal);
    assert!(
        !store.exists(),
        "the refusal came before the store was created"
    );

    let _ = granted_token(&store, "ada", TokenScope::HostWide);
    let before = spaced_out(&store);
    let too_far = grant_request(
        "ada",
        TokenScope::HostWide,
        Some(Duration::from_secs(u64::MAX)),
    );

    assert_eq!(answer_token_request(Some(&store), too_far), refusal);
    assert_eq!(
        std::fs::read(&store).expect("the store file is still there"),
        before,
        "the refused grant wrote nothing"
    );
}

#[test]
fn a_bare_revoke_stops_every_grant_the_identity_holds_in_the_stores_order() {
    let home = test_runtime_dir();
    let store = store_path(home.path());
    let session = SessionId::new();
    let _ = granted_token(&store, "ada", TokenScope::HostWide);
    let _ = granted_token(&store, "ada", TokenScope::Session(session));

    let before = SystemTime::now();
    let answer = answer_token_request(
        Some(&store),
        RouterRequestKind::RevokeToken {
            identity: "ada".to_string(),
            scope: None,
        },
    );
    let after = SystemTime::now();

    assert_eq!(
        answer,
        RouterResult::Revoked(vec![TokenScope::HostWide, TokenScope::Session(session)])
    );
    let written = TokenStore::read(&store).expect("the store reads back");
    assert_eq!(written.records.len(), 2);
    for record in &written.records {
        let stopped = record.revoked_at.expect("the revoke stamped this record");
        assert!(
            stopped >= before && stopped <= after,
            "the stamp is the clock reading the revoke took"
        );
    }
}

#[test]
fn a_scoped_revoke_stops_that_one_grant_and_leaves_the_other_standing() {
    let home = test_runtime_dir();
    let store = store_path(home.path());
    let session = SessionId::new();
    let _ = granted_token(&store, "ada", TokenScope::HostWide);
    let _ = granted_token(&store, "ada", TokenScope::Session(session));

    let before = SystemTime::now();
    let answer = answer_token_request(
        Some(&store),
        RouterRequestKind::RevokeToken {
            identity: "ada".to_string(),
            scope: Some(TokenScope::Session(session)),
        },
    );
    let after = SystemTime::now();

    assert_eq!(
        answer,
        RouterResult::Revoked(vec![TokenScope::Session(session)])
    );
    let written = TokenStore::read(&store).expect("the store reads back");
    assert_eq!(written.records.len(), 2);
    assert_eq!(written.records[0].scope, TokenScope::HostWide);
    assert_eq!(
        written.records[0].revoked_at, None,
        "the host-wide grant still stands"
    );
    assert_eq!(written.records[1].scope, TokenScope::Session(session));
    let stopped = written.records[1]
        .revoked_at
        .expect("the revoke stamped the session grant");
    assert!(
        stopped >= before && stopped <= after,
        "the stamp is the clock reading the revoke took"
    );
}

#[test]
fn revoking_an_identity_that_holds_nothing_stops_nothing_and_writes_nothing() {
    let home = test_runtime_dir();
    let store = store_path(home.path());
    let _ = granted_token(&store, "ada", TokenScope::HostWide);
    let before = spaced_out(&store);

    let answer = answer_token_request(
        Some(&store),
        RouterRequestKind::RevokeToken {
            identity: "grace".to_string(),
            scope: None,
        },
    );

    assert_eq!(answer, RouterResult::Revoked(Vec::new()));
    assert_eq!(
        std::fs::read(&store).expect("the store file is still there"),
        before,
        "a revoke that stopped nothing wrote nothing"
    );
}

#[test]
fn listing_answers_every_grant_without_its_hash_and_narrows_to_one_scope() {
    let home = test_runtime_dir();
    let store = store_path(home.path());
    let session = SessionId::new();
    let other = SessionId::new();
    let _ = granted_token(&store, "ada", TokenScope::HostWide);
    let _ = granted_token(&store, "ada", TokenScope::Session(session));
    let _ = granted_token(&store, "grace", TokenScope::Session(other));
    let written = TokenStore::read(&store).expect("the store reads back");
    let listed = |identity: &str, scope: &TokenScope| {
        let record = written
            .records
            .iter()
            .find(|record| record.identity == identity && record.scope == *scope)
            .expect("the store holds this grant");
        TokenEntry {
            identity: record.identity.clone(),
            scope: record.scope.clone(),
            issued_at: record.issued_at,
            expires_at: record.expires_at,
            last_used_at: record.last_used_at,
            revoked_at: record.revoked_at,
        }
    };

    let every = answer_token_request(Some(&store), RouterRequestKind::ListTokens { scope: None });

    assert_eq!(
        every,
        RouterResult::Tokens(vec![
            listed("ada", &TokenScope::HostWide),
            listed("ada", &TokenScope::Session(session)),
            listed("grace", &TokenScope::Session(other)),
        ])
    );
    let encoded = serde_json::to_string(&every).expect("the answer encodes");
    for record in &written.records {
        assert!(
            !encoded.contains(&record.hash),
            "a listed grant carries no hash"
        );
    }

    let narrowed = answer_token_request(
        Some(&store),
        RouterRequestKind::ListTokens {
            scope: Some(TokenScope::Session(session)),
        },
    );

    assert_eq!(
        narrowed,
        RouterResult::Tokens(vec![
            listed("ada", &TokenScope::HostWide),
            listed("ada", &TokenScope::Session(session)),
        ]),
        "one session lists every grant that reaches it, so ada's host-wide grant is listed \
         beside her grant on that session, and grace's grant on another session is not"
    );
}

#[test]
fn a_store_holding_junk_refuses_every_token_request_and_changes_nothing() {
    // One unreadable file refuses all three, so a grant can never write a
    // fresh store over records the router could not read.
    const JUNK: &[u8] = b"not a token store";
    let home = test_runtime_dir();
    let store = store_path(home.path());
    std::fs::create_dir_all(store.parent().expect("the store sits in a directory"))
        .expect("the store's directory is made");
    std::fs::write(&store, JUNK).expect("the junk is written");
    let error = TokenStore::read(&store).expect_err("junk is not a readable store");
    let refusal = token_refusal(&error.to_string());

    for kind in every_token_request(SessionId::new()) {
        let name = kind.name();
        assert_eq!(
            answer_token_request(Some(&store), kind),
            refusal,
            "{name} is refused"
        );
        assert_eq!(
            std::fs::read(&store).expect("the store file is still there"),
            JUNK,
            "{name} changed no byte of the store"
        );
    }
}

#[test]
fn a_machine_with_no_data_directory_refuses_every_token_request() {
    let refusal = token_refusal(
        "this machine has no data directory, so no remote access token can be stored",
    );

    for kind in every_token_request(SessionId::new()) {
        let name = kind.name();
        assert_eq!(
            answer_token_request(None, kind),
            refusal,
            "{name} is refused"
        );
    }
}

/// What [`read_ready_line`] reads from a child that printed `printed` on its
/// output. The bytes cross a real pipe, the way a session server's output
/// reaches the router.
#[cfg(unix)]
fn ready_line_from(printed: &str) -> Option<SessionServerReady> {
    let dir = test_runtime_dir();
    let path = dir.path().join("printed");
    std::fs::write(&path, printed).expect("the output is written");
    let mut child = std::process::Command::new("/bin/cat")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("cat runs");
    let stdout = child.stdout.take().expect("the child's output is piped");
    let read = read_ready_line(stdout);
    let _ = child.wait();
    read
}

#[cfg(unix)]
#[test]
fn the_line_a_session_server_prints_reads_back_as_its_report() {
    let report = SessionServerReady {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        socket: "/tmp/koshi-test.sock".to_string(),
    };
    let printed = serde_json::to_string(&report).expect("the report encodes");

    assert_eq!(ready_line_from(&format!("{printed}\n")), Some(report));
}

/// Every way the one line the router reads off a child's output fails to be a
/// report.
#[cfg(unix)]
#[test]
fn output_that_is_not_a_ready_report_reads_as_nothing() {
    assert_eq!(ready_line_from(""), None, "a child that printed nothing");
    assert_eq!(
        ready_line_from("not json\n"),
        None,
        "a line that is not a report"
    );
    assert_eq!(
        ready_line_from("{\"protocol_version\":1}\n"),
        None,
        "a report naming no socket"
    );
    assert_eq!(
        ready_line_from("{\"protocol_version\":\"one\",\"socket\":\"/tmp/s\"}\n"),
        None,
        "a report whose version is not a number"
    );
}

#[test]
fn a_session_server_on_this_build_is_served() {
    let report = SessionServerReady {
        protocol_version: ROUTER_PROTOCOL_VERSION,
        socket: "/tmp/koshi-test.sock".to_string(),
    };

    let accepted = accept_ready(Some(report.clone())).expect("this build's report is served");

    assert_eq!(accepted, report);
}

#[test]
fn a_session_server_that_printed_nothing_is_refused_as_no_bound_socket() {
    let refusal = accept_ready(None).expect_err("nothing readable is refused");

    assert_eq!(refusal, "the session did not report a bound socket");
}

#[test]
fn a_session_server_from_another_build_is_refused_naming_both_versions() {
    // The router spawns the koshi binary now on disk, so a binary swapped
    // under a running router reports a control-plane version this router does
    // not speak.
    let refusal = accept_ready(Some(SessionServerReady {
        protocol_version: ROUTER_PROTOCOL_VERSION + 1,
        socket: "/tmp/koshi-test.sock".to_string(),
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

/// How long a test waits for a connection the revoke cut to end.
const CUT_WAIT: Duration = Duration::from_secs(5);

/// How many loopback ports a test tries before it gives up opening the real
/// remote listener.
const ADDRESS_TRIES: usize = 8;

/// An address on the loopback interface nothing is listening on.
///
/// The port is taken and released, so the caller races every other program on
/// the machine for it. [`open_test_listener`] retries on that race.
fn free_loopback_address() -> String {
    let probe = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    probe
        .local_addr()
        .expect("the address that was bound")
        .to_string()
}

/// Open the real remote listener on a loopback port and hand back the address
/// it bound.
fn open_test_listener(cert: &CertFile, events_tx: &Sender<RouterEvent>) -> String {
    for _ in 0..ADDRESS_TRIES {
        let address = free_loopback_address();
        let opened = remote_listener::bind(address.clone(), cert);
        if let Ok(bound) = opened {
            bound.serve(events_tx.clone());
            return address;
        }
    }
    panic!("no loopback port could be bound in {ADDRESS_TRIES} tries");
}

/// Switch remote access on at a free loopback port, trying up to
/// [`ADDRESS_TRIES`] ports.
///
/// Sets `remote.address` to each port it tries and leaves it at the one that
/// worked.
fn enable_remote_on_a_free_port(
    remote: &mut RemoteState,
    events_tx: &Sender<RouterEvent>,
) -> RouterResult {
    for _ in 0..ADDRESS_TRIES {
        remote.address = Some(free_loopback_address());
        let answer = enable_remote(remote, events_tx);
        if matches!(answer, RouterResult::RemoteEnabled { .. }) {
            return answer;
        }
    }
    panic!("no loopback port could be enabled in {ADDRESS_TRIES} tries");
}

/// A stand-in session server behind the bridge, serving at `addr`: accept the
/// connection the router opens, answer the Hello the router presents on the
/// remote client's behalf, and hold the connection open until `stop` is
/// dropped.
fn bridged_session_server(addr: &str, stop: Receiver<()>) -> JoinHandle<()> {
    let listener = Listener::bind(addr).expect("bind the session behind the bridge");
    std::thread::spawn(move || {
        let mut connection = listener.accept().expect("accept the router's bridge");
        let hello: IpcRequest = connection
            .recv()
            .expect("read the hello the router presents");
        connection
            .send(&IpcResponse {
                request_id: Some(hello.request_id),
                result: IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
            })
            .expect("answer the hello");
        let _ = stop.recv();
    })
}

/// Write a token store at `path` holding one host-wide grant for alice that
/// never stops on its own, and hand back the secret it made.
fn store_with_alices_grant(path: &Path) -> ConnectionToken {
    let mut store = TokenStore::new();
    let (secret, _) = store.grant(
        "alice".to_string(),
        TokenScope::HostWide,
        SystemTime::now(),
        None,
    );
    store.write(path).expect("the token store is written");
    secret
}

/// Two ends of one loopback connection, for a test that needs a socket the
/// router can shut down.
fn loopback_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let address = listener.local_addr().expect("the address that was bound");
    let caller = TcpStream::connect(address).expect("the caller connects");
    let (served, _) = listener.accept().expect("the connection is accepted");
    (caller, served)
}

#[test]
fn a_revoke_ends_the_connection_it_admitted_attached_or_not() {
    // A connection that was admitted and never attached holds full access to
    // its scope until it ends: it lists the sessions on this machine and its
    // next attach is served. So the cut has to reach it, not only the
    // connections carrying a session's bytes.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let token_path = store_path(&data_dir);
    let session = SessionId::new();

    let secret = store_with_alices_grant(&token_path);
    let (cert, fingerprint) = load_or_make_cert(&data_dir).expect("this machine's certificate");

    let socket = socket_addr(runtime_dir.path(), session);
    let (stop_session, stopped) = mpsc::channel();
    let session_server = bridged_session_server(&socket, stopped);
    EndpointFile {
        socket,
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&EndpointFile::path(runtime_dir.path(), session))
    .expect("the endpoint file is written");

    let (events_tx, events_rx) = mpsc::channel();
    let address = open_test_listener(&cert, &events_tx);
    let sender = events_tx.clone();
    let held_runtime = runtime_dir.path().to_path_buf();
    let held_store = token_path.clone();
    let held_address = address.clone();
    let held_data = data_dir.clone();
    let loop_thread = std::thread::spawn(move || {
        let mut registry = registry_of(&[(session, "S-quiet-lake")]);
        let mut remote = RemoteState {
            address: Some(held_address),
            data_dir: Some(held_data),
            listening: true,
            live: Vec::new(),
            next_id: 0,
            said_full: Occasional::new(),
        };
        dispatch(
            &held_runtime,
            &test_exe(),
            Some(&held_store),
            &events_tx,
            &events_rx,
            TEST_IDLE_EXIT,
            &mut registry,
            &mut remote,
        )
    });

    // One connection attaches, so the router carries its session's bytes.
    let attaching = remote_client::connect(&address, &secret, Some(&fingerprint), DIAL_WAIT, None)
        .expect("the secret is admitted");
    let (mut bridged, _bridged_writer) =
        remote_client::attach_remote(attaching, SessionSelector::Id(session))
            .expect("the attach is sent");
    let answer: IncomingResponse = bridged.recv().expect("the session answers the hello");
    assert_eq!(answer.request_id, Some(1), "the bridge stands");

    // The other lists the sessions and then sits on the connection, exactly
    // as a client waiting for its user to pick one does.
    let mut listing =
        remote_client::connect(&address, &secret, Some(&fingerprint), DIAL_WAIT, None)
            .expect("the secret is admitted");
    let rows = remote_client::list_remote_sessions(&mut listing).expect("the sessions are listed");
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![session],
        "the host-wide grant reaches this machine's one session"
    );

    // Both connections block on their next read, as a client waiting on the
    // server does.
    let (bridged_ends, bridged_ended) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = bridged_ends.send(bridged.recv::<IncomingResponse>().is_err());
    });
    let mut listing_reader = listing.reader;
    let (listing_ends, listing_ended) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = listing_ends.send(listing_reader.recv::<RemoteServerFrame>().is_err());
    });

    let (reply, revoked) = mpsc::channel();
    sender
        .send(RouterEvent::Request {
            kind: RouterRequestKind::RevokeToken {
                identity: "alice".to_string(),
                scope: None,
            },
            reply,
        })
        .expect("the revoke is queued");
    assert_eq!(
        revoked.recv().expect("the revoke is answered"),
        RouterResult::Revoked(vec![TokenScope::HostWide])
    );

    assert!(
        listing_ended
            .recv_timeout(CUT_WAIT)
            .unwrap_or_else(|_| panic!(
                "the connection that only listed is still reading {CUT_WAIT:?} after the revoke"
            )),
        "the connection that only listed ended at the revoke"
    );
    assert!(
        bridged_ended
            .recv_timeout(CUT_WAIT)
            .unwrap_or_else(|_| panic!(
                "the connection carrying a session is still reading {CUT_WAIT:?} after the revoke"
            )),
        "the connection carrying a session ended at the revoke"
    );

    sender
        .send(RouterEvent::ChildExited(session))
        .expect("the exit is queued");
    assert_eq!(
        loop_thread.join().expect("the loop ended"),
        RouterExit::Idle
    );
    drop(stop_session);
    session_server.join().expect("the stand-in session ended");
}

#[test]
fn a_grant_cuts_only_the_standing_connection_of_its_identity_and_scope() {
    // Five records stand in the store; the grant for alice on `HostWide`
    // replaces exactly the one that is live on that identity and scope.
    // A revoked record, an expired one, another scope, and another identity
    // keep their connections.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let token_path = store_path(&data_dir);
    std::fs::create_dir_all(&data_dir).expect("create the data directory");

    let now = SystemTime::now();
    let hour = Duration::from_secs(3600);
    let record = |identity: &str,
                  hash: char,
                  scope: TokenScope,
                  expires_at: Option<SystemTime>,
                  revoked_at: Option<SystemTime>| TokenRecord {
        identity: identity.to_string(),
        hash: hash.to_string().repeat(64),
        scope,
        issued_at: now - hour,
        expires_at,
        last_used_at: None,
        revoked_at,
    };
    let other_session = SessionId::new();
    let mut store = TokenStore::new();
    store
        .records
        .push(record("alice", 'a', TokenScope::HostWide, None, None));
    store
        .records
        .push(record("alice", 'b', TokenScope::HostWide, None, Some(now)));
    store.records.push(record(
        "alice",
        'c',
        TokenScope::HostWide,
        Some(now - hour),
        None,
    ));
    store.records.push(record(
        "alice",
        'd',
        TokenScope::Session(other_session),
        None,
        None,
    ));
    store
        .records
        .push(record("bob", 'e', TokenScope::HostWide, None, None));
    store.write(&token_path).expect("the store is written");

    let mut remote = no_remote();
    let mut held_callers = Vec::new();
    for (index, hash) in ['a', 'b', 'c', 'd', 'e'].into_iter().enumerate() {
        let (caller, served) = loopback_pair();
        held_callers.push(caller);
        remote.live.push(LiveRemote {
            hash: hash.to_string().repeat(64),
            stream: served,
            id: index as u64,
        });
    }

    let result = grant_token(
        Some(&token_path),
        &mut remote,
        "alice".to_string(),
        TokenScope::HostWide,
        None,
    );

    let RouterResult::Granted { token, replaced } = result else {
        panic!("the grant was refused: {result:?}")
    };
    assert!(replaced, "the standing alice grant is reported replaced");
    let written = TokenStore::read(&token_path).expect("the store reads back");
    assert_eq!(
        written
            .records
            .iter()
            .filter(|record| record.identity == "alice" && record.scope == TokenScope::HostWide)
            .map(|record| record.hash.clone())
            .collect::<Vec<String>>(),
        vec![hash_token(&token)],
        "one alice record is left on the host-wide scope, holding the new secret"
    );
    let kept: Vec<String> = remote.live.iter().map(|live| live.hash.clone()).collect();
    assert_eq!(
        kept,
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
    // The attach is already on its way to the dispatcher when the revoke is
    // served. The cut drops the connection's registration, and that is what
    // the attach checks before it resolves the name the caller sent.
    let runtime_dir = test_runtime_dir();
    let session = SessionId::new();
    let registry = registry_of(&[(session, "S-quiet-lake")]);
    let hash = "b".repeat(64);
    let mut remote = no_remote();
    let (_caller, served) = loopback_pair();
    remote.live.push(LiveRemote {
        hash: hash.clone(),
        stream: served,
        id: 7,
    });

    let before = locate_remote(
        runtime_dir.path(),
        &registry,
        &remote,
        &TokenScope::HostWide,
        7,
        &SessionSelector::Id(session),
    );
    remote.cut(&[hash]);
    let after = locate_remote(
        runtime_dir.path(),
        &registry,
        &remote,
        &TokenScope::HostWide,
        7,
        &SessionSelector::Id(session),
    );

    assert_eq!(
        before,
        Some(EndpointFile::path(runtime_dir.path(), session)),
        "the same attach reached the session while the connection stood"
    );
    assert_eq!(after, None, "the cut connection reaches nothing");
    assert!(
        remote.live.is_empty(),
        "the cut connection left the list, so nothing later matches its number"
    );
}

#[test]
fn a_secret_on_one_session_is_shown_that_session_and_no_other() {
    // A grant on one session is the whole reach of the secret behind it. The
    // listing is the first thing an admitted caller asks for, so a session
    // outside the grant must not even be named back.
    let reached = SessionId::new();
    let beside_it = SessionId::new();
    let registry = registry_of(&[(reached, "S-quiet-lake"), (beside_it, "S-loud-river")]);

    assert_eq!(
        remote_rows(&registry, &TokenScope::Session(reached)),
        vec![RemoteSessionRow {
            id: reached,
            name: "S-quiet-lake".to_string(),
        }]
    );
    assert_eq!(
        remote_rows(&registry, &TokenScope::Session(SessionId::new())),
        Vec::<RemoteSessionRow>::new(),
        "a grant on a session this machine does not run is shown nothing"
    );
    assert_eq!(
        remote_rows(&registry, &TokenScope::HostWide),
        vec![
            RemoteSessionRow {
                id: beside_it,
                name: "S-loud-river".to_string(),
            },
            RemoteSessionRow {
                id: reached,
                name: "S-quiet-lake".to_string(),
            },
        ],
        "a host-wide grant is shown every session, in name order"
    );
    assert_eq!(
        remote_rows(&Registry::new(), &TokenScope::HostWide),
        Vec::<RemoteSessionRow>::new(),
        "and a machine running nothing is shown nothing"
    );
}

#[test]
fn two_sessions_carrying_one_name_are_listed_in_id_order() {
    // Names come from a walk that never hands out a name the list holds, so
    // two sessions share one only when one of them was started by another
    // local user. The id settles the order, and without it the answer is
    // whatever order the list happens to be in.
    let mut twins = [SessionId::new(), SessionId::new()];
    twins.sort();
    let [first, second] = twins;
    let registry = registry_of(&[(second, "S-quiet-lake"), (first, "S-quiet-lake")]);

    assert_eq!(
        remote_rows(&registry, &TokenScope::HostWide),
        vec![
            RemoteSessionRow {
                id: first,
                name: "S-quiet-lake".to_string(),
            },
            RemoteSessionRow {
                id: second,
                name: "S-quiet-lake".to_string(),
            },
        ]
    );
}

#[test]
fn an_attach_to_a_session_the_secret_does_not_reach_is_refused() {
    // The connection stands and the session is running, so the scope is the
    // one thing that refuses this attach. Without it a grant on one session
    // would carry a caller into every session on the machine.
    let reached = SessionId::new();
    let beside_it = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let registry = registry_of(&[(reached, "S-quiet-lake"), (beside_it, "S-loud-river")]);
    let mut remote = no_remote();
    let (_caller, served) = loopback_pair();
    remote.live.push(LiveRemote {
        hash: "c".repeat(64),
        stream: served,
        id: 3,
    });
    let located = |scope: &TokenScope, selector: &SessionSelector| {
        locate_remote(runtime_dir.path(), &registry, &remote, scope, 3, selector)
    };

    assert_eq!(
        located(&TokenScope::Session(reached), &SessionSelector::Id(reached)),
        Some(EndpointFile::path(runtime_dir.path(), reached)),
        "the session the grant names is reached"
    );
    assert_eq!(
        located(
            &TokenScope::Session(reached),
            &SessionSelector::Id(beside_it)
        ),
        None,
        "the session beside it is outside the grant"
    );
    assert_eq!(
        located(
            &TokenScope::Session(reached),
            &SessionSelector::Name("S-loud-river".to_string())
        ),
        None,
        "and naming that session instead reaches nothing either"
    );
    assert_eq!(
        located(&TokenScope::HostWide, &SessionSelector::Id(beside_it)),
        Some(EndpointFile::path(runtime_dir.path(), beside_it)),
        "a host-wide grant reaches both"
    );
}

#[test]
fn an_attach_naming_a_session_this_machine_does_not_run_reaches_nothing() {
    // The connection stands and a host-wide grant reaches every session on
    // this machine. The selector is the one thing left that refuses the
    // attach.
    let running = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let registry = registry_of(&[(running, "S-quiet-lake")]);
    let mut remote = no_remote();
    let (_caller, served) = loopback_pair();
    remote.live.push(LiveRemote {
        hash: "f".repeat(64),
        stream: served,
        id: 9,
    });
    let located = |selector: &SessionSelector| {
        locate_remote(
            runtime_dir.path(),
            &registry,
            &remote,
            &TokenScope::HostWide,
            9,
            selector,
        )
    };

    assert_eq!(
        located(&SessionSelector::Name("S-loud-river".to_string())),
        None,
        "a name the list does not hold"
    );
    assert_eq!(
        located(&SessionSelector::Name("S-quiet".to_string())),
        None,
        "a name that is only the start of one the list holds"
    );
    assert_eq!(
        located(&SessionSelector::Id(SessionId::new())),
        None,
        "an id the list does not hold"
    );
    assert_eq!(
        located(&SessionSelector::Id(running)),
        Some(EndpointFile::path(runtime_dir.path(), running)),
        "and the session that is running is still reached"
    );
}

#[test]
fn a_session_another_local_user_started_is_neither_listed_nor_reached_from_a_remote_connection() {
    // The rebuild registers those sessions from the machine-wide shared
    // directory, under `pid` 0, and the bridge reads the endpoint file under
    // this router's own runtime directory, which holds none of them. Listing
    // one would name a session every attach then refuses, and would carry
    // another local user's session name and id out over the network.
    let own = SessionId::new();
    let foreign = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let mut registry = registry_of(&[(own, "S-quiet-lake")]);
    registry.insert(
        foreign,
        SessionEntry {
            name: "S-loud-river".to_string(),
            socket: socket_addr(Path::new("/nowhere"), foreign),
            pid: 0,
        },
    );
    let mut remote = no_remote();
    let (_caller, served) = loopback_pair();
    remote.live.push(LiveRemote {
        hash: "a".repeat(64),
        stream: served,
        id: 5,
    });
    let located = |selector: &SessionSelector| {
        locate_remote(
            runtime_dir.path(),
            &registry,
            &remote,
            &TokenScope::HostWide,
            5,
            selector,
        )
    };

    assert_eq!(
        remote_rows(&registry, &TokenScope::HostWide),
        vec![RemoteSessionRow {
            id: own,
            name: "S-quiet-lake".to_string(),
        }],
        "a host-wide grant is shown the session this router started and no other"
    );
    assert_eq!(
        remote_rows(&registry, &TokenScope::Session(foreign)),
        Vec::<RemoteSessionRow>::new(),
        "and a grant naming that session outright is shown nothing"
    );
    assert_eq!(
        located(&SessionSelector::Id(foreign)),
        None,
        "an attach naming it by id reaches nothing"
    );
    assert_eq!(
        located(&SessionSelector::Name("S-loud-river".to_string())),
        None,
        "and naming it reaches nothing either"
    );
    assert_eq!(
        located(&SessionSelector::Id(own)),
        Some(EndpointFile::path(runtime_dir.path(), own)),
        "while the session this router started is still reached"
    );
}

#[test]
fn the_report_that_one_connection_ended_drops_that_registration_and_no_other() {
    // The listener sends this when a remote connection closes. The place it
    // frees lets the next caller in, and the numbers beside it have to
    // survive: a later attach finds its connection by number.
    let runtime_dir = test_runtime_dir();
    let mut remote = no_remote();
    let mut held = Vec::new();
    for id in 0..3u64 {
        let (near, far) = loopback_pair();
        held.push(far);
        remote.live.push(LiveRemote {
            hash: "g".repeat(64),
            stream: near,
            id,
        });
    }

    serve_admission(
        runtime_dir.path(),
        None,
        &Registry::new(),
        &mut remote,
        AdmissionAsk::Ended { id: 1 },
    );

    assert_eq!(
        remote.live.iter().map(|live| live.id).collect::<Vec<u64>>(),
        vec![0, 2]
    );
}

#[test]
fn a_caller_speaking_no_doorway_version_this_build_has_is_told_both_ranges() {
    // The version is settled before the secret is looked at, so this needs no
    // grant and no dispatcher. This refusal names both ranges instead of
    // carrying REMOTE_REFUSED.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let (cert, fingerprint) = load_or_make_cert(&data_dir).expect("this machine's certificate");

    let (events_tx, _events_rx) = mpsc::channel();
    let address = open_test_listener(&cert, &events_tx);

    let ahead = REMOTE_PROTOCOL_VERSION + 1;
    let hello = RemoteClientFrame::Hello {
        min_remote_version: ahead,
        max_remote_version: ahead + 1,
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        token: ConnectionToken::generate(),
    };
    let (_reader, _writer, _presented, answer) =
        remote_wire::open(&address, Some(&fingerprint), &hello, DIAL_WAIT, None)
            .expect("the server answers the opening frame");

    let RemoteServerFrame::Refused { message } = answer else {
        panic!("a doorway version with no overlap is refused, and got {answer:?}");
    };
    assert_eq!(
        message,
        format!(
            "the caller speaks remote doorway {ahead} to {}, this koshi speaks \
             {MIN_REMOTE_PROTOCOL_VERSION} to {REMOTE_PROTOCOL_VERSION}",
            ahead + 1
        )
    );
    assert_ne!(
        message, REMOTE_REFUSED,
        "a version refusal is not the sentence a wrong secret gets"
    );
}

#[test]
fn a_caller_whose_doorway_range_covers_this_build_settles_on_what_both_speak() {
    // The overlap is the highest version both ends hold, and the Welcome names
    // it so the caller knows what it is talking to.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let token_path = store_path(&data_dir);

    let secret = store_with_alices_grant(&token_path);
    let (cert, fingerprint) = load_or_make_cert(&data_dir).expect("this machine's certificate");

    let (events_tx, events_rx) = mpsc::channel();
    let address = open_test_listener(&cert, &events_tx);
    let held_runtime = runtime_dir.path().to_path_buf();
    let held_store = token_path.clone();
    let held_address = address.clone();
    let held_data = data_dir.clone();
    let loop_thread = std::thread::spawn(move || {
        let mut registry = registry_of(&[]);
        let mut remote = RemoteState {
            address: Some(held_address),
            data_dir: Some(held_data),
            listening: true,
            live: Vec::new(),
            next_id: 0,
            said_full: Occasional::new(),
        };
        dispatch(
            &held_runtime,
            &test_exe(),
            Some(&held_store),
            &events_tx,
            &events_rx,
            TEST_IDLE_EXIT,
            &mut registry,
            &mut remote,
        )
    });

    // A caller that speaks this build's version and one above it.
    let hello = RemoteClientFrame::Hello {
        min_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        max_remote_version: REMOTE_PROTOCOL_VERSION + 1,
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        token: secret.clone(),
    };
    let (_reader, _writer, _presented, answer) =
        remote_wire::open(&address, Some(&fingerprint), &hello, DIAL_WAIT, None)
            .expect("the server answers the opening frame");

    assert_eq!(
        answer,
        RemoteServerFrame::Welcome {
            remote_version: REMOTE_PROTOCOL_VERSION
        },
        "the settled version is the highest both ends speak, not the caller's highest"
    );

    drop(loop_thread);
}

#[test]
fn an_admitted_secret_is_registered_with_its_scope_and_stamped_in_the_store() {
    // The registration is what a later attach and a later revoke both find the
    // connection by, and the stamp is what `koshi share list` reads as the
    // last time that grant was used.
    let runtime_dir = test_runtime_dir();
    let token_path = store_path(&runtime_dir.path().join("data"));
    let secret = store_with_alices_grant(&token_path);
    let mut remote = no_remote();
    let (near, _far) = loopback_pair();

    let before = SystemTime::now();
    let admitted =
        admit_token(Some(&token_path), &mut remote, &secret, near).expect("the secret is admitted");
    let after = SystemTime::now();

    assert_eq!(admitted.scope, TokenScope::HostWide);
    assert_eq!(admitted.id, 0);
    assert_eq!(remote.next_id, 1, "the next connection takes the number 1");
    assert_eq!(remote.live.len(), 1);
    assert_eq!(remote.live[0].id, 0);
    assert_eq!(
        remote.live[0].hash,
        hash_token(&secret),
        "the connection is registered against the hash of the secret that opened it"
    );
    let written = TokenStore::read(&token_path).expect("the store reads back");
    assert_eq!(written.records.len(), 1);
    let used = written.records[0]
        .last_used_at
        .expect("the admit stamped the record");
    assert!(
        used >= before && used <= after,
        "the stamp is the clock reading the admit took"
    );
}

#[test]
fn a_secret_the_store_does_not_hold_admits_nothing_and_writes_nothing() {
    // A wrong secret must leave no trace: nothing registered, and no write
    // that would stamp a record nobody used.
    let runtime_dir = test_runtime_dir();
    let token_path = store_path(&runtime_dir.path().join("data"));
    let _ = store_with_alices_grant(&token_path);
    let before = spaced_out(&token_path);
    let mut remote = no_remote();
    let (near, _far) = loopback_pair();

    assert!(
        admit_token(
            Some(&token_path),
            &mut remote,
            &ConnectionToken::generate(),
            near
        )
        .is_none(),
        "a secret no record holds reaches nothing"
    );

    assert!(remote.live.is_empty(), "nothing was registered for it");
    assert_eq!(remote.next_id, 0, "and it took no number");
    assert_eq!(
        std::fs::read(&token_path).expect("the store file is still there"),
        before,
        "the refused secret wrote nothing"
    );
}

#[test]
fn a_machine_with_no_token_store_admits_no_remote_connection() {
    // With no data directory there is no store to check a secret against, and
    // every remote caller is refused.
    let mut remote = no_remote();
    let (near, _far) = loopback_pair();

    assert!(admit_token(None, &mut remote, &ConnectionToken::generate(), near).is_none());

    assert!(remote.live.is_empty(), "nothing was registered for it");
    assert_eq!(remote.next_id, 0, "and it took no number");
}

#[test]
fn a_full_list_of_admitted_connections_admits_nothing_more() {
    // One valid secret, admitted MAX_LIVE_REMOTE times, then refused.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let token_path = store_path(&data_dir);

    let secret = store_with_alices_grant(&token_path);

    let mut remote = no_remote();
    let mut held = Vec::new();
    for place in 0..MAX_LIVE_REMOTE {
        let (near, far) = loopback_pair();
        held.push(far);
        let admitted = admit_token(Some(&token_path), &mut remote, &secret, near)
            .unwrap_or_else(|| panic!("place {place} of {MAX_LIVE_REMOTE} is free"));
        assert_eq!(admitted.scope, TokenScope::HostWide);
        assert_eq!(
            admitted.id, place as u64,
            "each admitted connection takes the next number"
        );
    }
    assert_eq!(remote.live.len(), MAX_LIVE_REMOTE);

    let (near, _far) = loopback_pair();
    assert!(
        admit_token(Some(&token_path), &mut remote, &secret, near).is_none(),
        "a good secret arriving at a full list is refused"
    );
    assert_eq!(
        remote.live.len(),
        MAX_LIVE_REMOTE,
        "and nothing was registered for it"
    );
    assert_eq!(
        remote.next_id, MAX_LIVE_REMOTE as u64,
        "and the refused connection took no number"
    );
}

#[test]
fn a_connection_that_ends_makes_room_for_the_next_one() {
    // An `Ended` report drops a registration and frees its place.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let token_path = store_path(&data_dir);

    let secret = store_with_alices_grant(&token_path);

    let mut remote = no_remote();
    let mut held = Vec::new();
    let mut first = None;
    for _ in 0..MAX_LIVE_REMOTE {
        let (near, far) = loopback_pair();
        held.push(far);
        let admitted = admit_token(Some(&token_path), &mut remote, &secret, near)
            .expect("the list starts empty");
        first.get_or_insert(admitted.id);
    }
    let (near, _far) = loopback_pair();
    assert!(admit_token(Some(&token_path), &mut remote, &secret, near).is_none());

    let ended = first.expect("a full list has a first connection");
    serve_admission(
        runtime_dir.path(),
        Some(&token_path),
        &Registry::new(),
        &mut remote,
        AdmissionAsk::Ended { id: ended },
    );
    assert_eq!(remote.live.len(), MAX_LIVE_REMOTE - 1);

    let (near, _far) = loopback_pair();
    let admitted = admit_token(Some(&token_path), &mut remote, &secret, near)
        .expect("the place it left is free");
    assert_eq!(
        admitted.id, MAX_LIVE_REMOTE as u64,
        "the connection taking that place takes the next number, not the freed one"
    );
}

#[test]
fn switching_remote_access_on_with_no_listen_address_is_refused() {
    // `koshi.kdl` names where the port would be. With no address the refusal
    // names the line to add.
    let (events_tx, _events_rx) = mpsc::channel();
    let mut remote = no_remote();

    let answer = enable_remote(&mut remote, &events_tx);

    assert_eq!(
        answer,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "no remote listen address is set; add `remote-listen \"<host:port>\"` to \
                      koshi.kdl"
                .to_string(),
        })
    );
    assert!(!remote.listening, "nothing was taken");
}

#[test]
fn switching_remote_access_on_with_no_data_directory_is_refused() {
    // The certificate and the record of the answer both live in the data
    // directory. A machine with none holds neither.
    let (events_tx, _events_rx) = mpsc::channel();
    let mut remote = no_remote();
    remote.address = Some("127.0.0.1:7654".to_string());

    let answer = enable_remote(&mut remote, &events_tx);

    assert_eq!(
        answer,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: "this machine has no data directory, so remote access cannot be switched on"
                .to_string(),
        })
    );
    assert!(!remote.listening, "nothing was taken");
}

#[test]
fn switching_remote_access_on_while_this_router_already_holds_the_port_keeps_serving_on_it() {
    // The listener opened at start-up, and the bind is skipped. An address
    // something else holds is not a refusal here.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let (_cert, fingerprint) = load_or_make_cert(&data_dir).expect("this machine's certificate");
    let occupied = TcpListener::bind("127.0.0.1:0").expect("hold a loopback address");
    let address = occupied
        .local_addr()
        .expect("read the held address")
        .to_string();
    let (events_tx, _events_rx) = mpsc::channel();
    let mut remote = RemoteState {
        address: Some(address.clone()),
        data_dir: Some(data_dir.clone()),
        listening: true,
        live: Vec::new(),
        next_id: 0,
        said_full: Occasional::new(),
    };

    let answer = enable_remote(&mut remote, &events_tx);

    assert_eq!(
        answer,
        RouterResult::RemoteEnabled {
            address,
            fingerprint,
        }
    );
    assert!(remote.listening, "the port it already held stays open");
    assert!(
        remote_enabled(&data_dir),
        "the answer is written down, so the next start opens the port again"
    );

    drop(occupied);
}

#[test]
fn the_start_up_open_with_no_listen_address_takes_no_port() {
    let (events_tx, _events_rx) = mpsc::channel();
    let mut remote = no_remote();

    open_remote_listener(&mut remote, &events_tx);

    assert!(!remote.listening, "no address, so no port");
}

#[test]
fn the_start_up_open_takes_no_port_until_the_operator_has_said_yes() {
    // An address alone opens nothing: the record beside the certificate is
    // what a start reads as the operator's yes.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let (events_tx, _events_rx) = mpsc::channel();
    let mut remote = RemoteState {
        address: Some(free_loopback_address()),
        data_dir: Some(data_dir.clone()),
        listening: false,
        live: Vec::new(),
        next_id: 0,
        said_full: Occasional::new(),
    };

    open_remote_listener(&mut remote, &events_tx);

    assert!(!remote.listening, "no record of a yes, so no port");
    assert!(
        !CertFile::path(&data_dir).exists(),
        "the open stopped before it made this machine's certificate"
    );
}

#[test]
fn the_start_up_open_takes_the_port_again_once_the_answer_is_written_down() {
    // The record beside the certificate opens the port on every start after
    // the one that wrote it, with nobody asked again.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    EnabledFile {
        format: ENABLED_FILE_FORMAT,
        enabled_at: SystemTime::now(),
    }
    .write(&EnabledFile::path(&data_dir))
    .expect("the answer is written");
    let (events_tx, _events_rx) = mpsc::channel();
    let mut remote = RemoteState {
        address: None,
        data_dir: Some(data_dir),
        listening: false,
        live: Vec::new(),
        next_id: 0,
        said_full: Occasional::new(),
    };

    for _ in 0..ADDRESS_TRIES {
        remote.address = Some(free_loopback_address());
        open_remote_listener(&mut remote, &events_tx);
        if remote.listening {
            break;
        }
    }

    assert!(
        remote.listening,
        "no loopback port could be opened in {ADDRESS_TRIES} tries"
    );
    let address = remote.address.clone().expect("the address it took");
    assert_eq!(
        TcpListener::bind(&address)
            .expect_err("the router is holding the address")
            .kind(),
        std::io::ErrorKind::AddrInUse
    );
}

#[test]
fn an_address_that_cannot_be_taken_writes_no_record_of_the_answer() {
    // The operator says yes, the address is already held by something else,
    // and the answer must not survive: a record written here would open the
    // port on the next start with nobody asked again, while the operator was
    // just told it did not work.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");

    // Hold the address so the router cannot take it.
    let occupied = TcpListener::bind("127.0.0.1:0").expect("hold a loopback address");
    let address = occupied.local_addr().expect("read the held address");

    let (events_tx, _events_rx) = mpsc::channel();
    let mut remote = RemoteState {
        address: Some(address.to_string()),
        data_dir: Some(data_dir.clone()),
        listening: false,
        live: Vec::new(),
        next_id: 0,
        said_full: Occasional::new(),
    };

    let held = TcpListener::bind(address).expect_err("the address is already held");
    let answer = enable_remote(&mut remote, &events_tx);

    assert_eq!(
        answer,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!("the remote listener could not open {address}: {held}"),
        })
    );
    assert!(
        !remote.listening,
        "nothing is being served on an address that was never taken"
    );
    assert!(
        !remote_enabled(&data_dir),
        "no record of the answer survives, so the next start opens nothing"
    );
    assert!(
        !EnabledFile::path(&data_dir).exists(),
        "and the record was never written at all"
    );

    drop(occupied);
}

#[test]
fn taking_the_address_writes_the_record_and_serves_on_it() {
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");

    let (events_tx, _events_rx) = mpsc::channel();
    let mut remote = RemoteState {
        address: None,
        data_dir: Some(data_dir.clone()),
        listening: false,
        live: Vec::new(),
        next_id: 0,
        said_full: Occasional::new(),
    };

    let answer = enable_remote_on_a_free_port(&mut remote, &events_tx);

    let RouterResult::RemoteEnabled {
        address: served,
        fingerprint,
    } = answer
    else {
        panic!("an address that can be taken is enabled, and got {answer:?}");
    };
    assert_eq!(served, remote.address.clone().expect("the address it took"));
    let (_cert, on_disk) = load_or_make_cert(&data_dir).expect("this machine's certificate");
    assert_eq!(
        fingerprint, on_disk,
        "the answer names the certificate this machine now presents"
    );
    assert!(remote.listening, "the port is being served");
    assert!(
        remote_enabled(&data_dir),
        "the answer is written down, so the next start opens the port again"
    );
}

#[test]
fn the_status_separates_the_answer_given_from_the_port_being_open() {
    // A machine whose address is held by something else has said yes and has
    // no port. Reporting only the answer would hand out a connect line for an
    // address nothing replies on.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    EnabledFile {
        format: ENABLED_FILE_FORMAT,
        enabled_at: SystemTime::now(),
    }
    .write(&EnabledFile::path(&data_dir))
    .expect("the answer is written");

    let remote = RemoteState {
        address: Some("127.0.0.1:7654".to_string()),
        data_dir: Some(data_dir),
        listening: false,
        live: Vec::new(),
        next_id: 0,
        said_full: Occasional::new(),
    };

    let RouterResult::RemoteStatus {
        enabled, listening, ..
    } = remote_status(&remote)
    else {
        panic!("a status request is answered with a status");
    };
    assert!(enabled, "the operator did say yes");
    assert!(!listening, "and this run is holding no port");
}

#[test]
fn the_status_names_this_machines_certificate_and_how_many_connections_it_holds() {
    // The operator reads the fingerprint to hand to a caller, and the count to
    // decide whether a revoke is worth making. A machine holding a certificate
    // it made has still not said yes until the record is written.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let (_cert, fingerprint) = load_or_make_cert(&data_dir).expect("this machine's certificate");
    let mut remote = RemoteState {
        address: Some("127.0.0.1:7654".to_string()),
        data_dir: Some(data_dir),
        listening: true,
        live: Vec::new(),
        next_id: 0,
        said_full: Occasional::new(),
    };
    let _far_ends: Vec<TcpStream> = (0..3u64)
        .map(|id| {
            let (near, far) = loopback_pair();
            remote.live.push(LiveRemote {
                hash: "d".repeat(64),
                stream: near,
                id,
            });
            far
        })
        .collect();

    assert_eq!(
        remote_status(&remote),
        RouterResult::RemoteStatus {
            address: Some("127.0.0.1:7654".to_string()),
            enabled: false,
            listening: true,
            fingerprint: Some(fingerprint),
            remote_connections: Some(3),
        }
    );
}

#[test]
fn a_listener_that_cannot_start_serving_writes_no_record_of_the_answer() {
    // Taking the port and starting its thread both happen before the record is
    // written, and serving cannot fail after it. So there is no ordering left
    // in which the record outlives a listener that never opened, which the
    // next start would read as an answer nobody gave again.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let occupied = TcpListener::bind("127.0.0.1:0").expect("hold a loopback address");
    let address = occupied.local_addr().expect("read the held address");

    let (events_tx, _events_rx) = mpsc::channel();
    let mut remote = RemoteState {
        address: Some(address.to_string()),
        data_dir: Some(data_dir.clone()),
        listening: false,
        live: Vec::new(),
        next_id: 0,
        said_full: Occasional::new(),
    };

    let held = TcpListener::bind(address).expect_err("the address is already held");
    assert_eq!(
        enable_remote(&mut remote, &events_tx),
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!("the remote listener could not open {address}: {held}"),
        })
    );
    assert!(!EnabledFile::path(&data_dir).exists());
    assert!(!remote.listening);

    drop(occupied);
}

#[test]
fn a_bound_port_that_is_never_served_is_given_back() {
    // The record write sits between taking the port and serving on it, and a
    // write that fails drops the port. This is what makes that drop real: the
    // same address binds again straight after.
    let runtime_dir = test_runtime_dir();
    let data_dir = runtime_dir.path().join("data");
    let (cert, _) = load_or_make_cert(&data_dir).expect("this machine's certificate");
    let address = free_loopback_address();

    let bound = remote_listener::bind(address.clone(), &cert).expect("the port is taken");
    drop(bound);

    // The thread the bind started ends when its sender goes away, and the port
    // goes with it.
    let mut freed = false;
    for _ in 0..50 {
        if TcpListener::bind(&address).is_ok() {
            freed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(freed, "a port that was never served is free again");
}
