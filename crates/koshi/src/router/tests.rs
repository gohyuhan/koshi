//! Tests for the router's session list and its dispatcher loop, run in
//! process against a hand-built list: no router is bound and no session
//! server is started, so the name walk, selector resolution, removal, and the
//! idle-exit rule are exercised on their own. Starting a real router and a
//! real session server needs whole processes, so that is covered by the
//! integration tests instead.

use super::*;

use std::time::UNIX_EPOCH;

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

    assert_eq!(sweep(runtime_dir.path()), Registry::new());
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

    assert_eq!(sweep(runtime_dir.path()), Registry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

/// A short idle window, so an idle-exit test finishes quickly.
const TEST_IDLE_EXIT: Duration = Duration::from_millis(50);

#[test]
fn an_idle_window_that_passes_with_no_session_running_ends_the_loop() {
    let runtime_dir = test_runtime_dir();
    let (events_tx, events_rx) = mpsc::channel();

    let left = dispatch(
        runtime_dir.path().to_path_buf(),
        events_tx,
        events_rx,
        TEST_IDLE_EXIT,
        Registry::new(),
    );

    assert_eq!(left, Registry::new());
}

#[test]
fn a_request_inside_the_idle_window_is_served_and_the_loop_goes_on() {
    // A create arriving just as the router would have exited must still be
    // answered, so a caller's first request is never dropped on the floor.
    let runtime_dir = test_runtime_dir();
    let (events_tx, events_rx) = mpsc::channel();
    let (reply, answer) = mpsc::channel();
    events_tx
        .send(RouterEvent::Request {
            kind: RouterRequestKind::ListSessions,
            reply,
        })
        .expect("the request is queued");

    let left = dispatch(
        runtime_dir.path().to_path_buf(),
        events_tx,
        events_rx,
        TEST_IDLE_EXIT,
        Registry::new(),
    );

    assert_eq!(
        answer.try_recv().expect("the loop answered the request"),
        RouterResult::Sessions(Vec::new())
    );
    assert_eq!(left, Registry::new());
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
        dispatch(
            held,
            events_tx,
            events_rx,
            TEST_IDLE_EXIT,
            registry_of(&[(running, "S-quiet-lake")]),
        )
    });

    std::thread::sleep(TEST_IDLE_EXIT * 5);
    assert!(
        !loop_thread.is_finished(),
        "the loop is still serving while a session is running"
    );

    sender
        .send(RouterEvent::ChildExited(running))
        .expect("the exit is queued");
    let left = loop_thread.join().expect("the loop ended");

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
        dispatch(
            held,
            events_tx,
            events_rx,
            TEST_IDLE_EXIT,
            registry_of(&[(gone, "S-quiet-lake"), (stays, "S-loud-river")]),
        )
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
    let left = loop_thread.join().expect("the loop ended");

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
            code: IpcErrorCode::MalformedRequest,
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
            code: IpcErrorCode::MalformedRequest,
            message: format!("no session {dead} is running"),
        })
    );
    assert_eq!(registry, Registry::new());
    assert!(!endpoint_path.exists(), "the endpoint file is removed");
}

#[test]
fn a_kill_for_a_session_the_list_does_not_hold_is_refused_by_id() {
    let absent = SessionId::new();
    let runtime_dir = test_runtime_dir();
    let mut registry = Registry::new();

    let answer = kill_session(
        runtime_dir.path(),
        &mut registry,
        &SessionSelector::Id(absent),
    );

    assert_eq!(
        answer,
        RouterResult::Error(IpcErrorPayload {
            code: IpcErrorCode::MalformedRequest,
            message: format!("no session {absent} is running"),
        })
    );
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
    )
    .expect("the command is built");

    assert_eq!(command.get_current_dir(), Some(dir.path()));
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
    )
    .expect("the command is built");

    assert_eq!(command.get_current_dir(), None);
}
