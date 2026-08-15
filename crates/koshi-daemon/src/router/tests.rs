//! Tests for the router's session list and its dispatcher loop, run in
//! process against a hand-built list: no router is bound and no session
//! server is started, so the name walk, selector resolution, removal, the
//! idle-exit rule, the lock handover, the answer to a restart request, and the
//! three remote access token requests are exercised on their own. Starting a
//! real router and a real session server needs whole processes, so that is
//! covered by the integration tests instead.

use super::*;

use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use koshi_core::discovery::{SessionInfo, SessionOverview};
use koshi_ipc::endpoint::RESTART_WINDOW;
use koshi_ipc::endpoint::{advert_path, shared_socket_addr};
use koshi_ipc::protocol::{IpcRequest, IpcResponse, IpcResult, PROTOCOL_VERSION};
use koshi_ipc::remote_tokens::{hash_token, TokenEntry, TOKEN_STORE_FORMAT};
use tempfile::TempDir;

/// A fresh directory to stand in for the runtime dir, under a short base so
/// the Unix socket path stays inside the OS path-length cap. Removed when the
/// test drops it.
fn test_runtime_dir() -> TempDir {
    #[cfg(unix)]
    let base = PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    TempDir::new_in(base).expect("a temporary runtime directory")
}

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
                    created_at: UNIX_EPOCH,
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
    // A name is what a caller types, so a prefix of one must not resolve —
    // `koshi attach S-quiet` would otherwise land on `S-quiet-lake`.
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
fn the_rebuild_removes_a_resume_file_no_session_claims() {
    // The new image never started, so nothing deleted the file and no endpoint
    // file is left to walk it from. Without this the file stays on the disk for
    // as long as the machine does.
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
                created_at,
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

/// A process this test is the parent of, which ends at once. The handle is
/// dropped without waiting on it, so the exit is left for the watcher to
/// collect.
#[cfg(unix)]
fn short_lived_child() -> u32 {
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the shell runs")
        .id()
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

    watch_session_exit(short_lived_child(), id, events_tx);

    match events_rx.recv() {
        Ok(RouterEvent::ChildExited(reported)) => assert_eq!(reported, id),
        Ok(_) => panic!("the watcher reported something other than the session's exit"),
        Err(mpsc::RecvError) => panic!("the watcher ended without reporting the exit"),
    }
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

/// A serving thread's SIGPIPE block must hold while the process-wide
/// disposition sits at its default, the state a running exec puts it in. The
/// raise is thread-directed, like the signal a write to a hung-up peer
/// raises; blocked, it stays pending, the thread runs on, and the pending
/// signal dies with the thread.
#[cfg(unix)]
#[test]
fn a_serving_threads_sigpipe_block_holds_under_the_default_disposition() {
    let survived = std::thread::spawn(|| {
        block_sigpipe_on_this_thread();
        let prior = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let raised = unsafe { libc::raise(libc::SIGPIPE) };
        unsafe { libc::signal(libc::SIGPIPE, prior) };
        raised == 0
    })
    .join()
    .expect("the thread survives the raised SIGPIPE");
    assert!(survived, "the raise itself reported an error");
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
