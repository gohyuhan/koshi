//! Tests for the attached client: which session a bare `koshi attach` settles on
//! from a listing, that joining by id asks the router nothing, how one frame
//! read from the event stream decides whether the loop keeps reading and, when
//! it does not, how it ended, and the mouse path — what the pile folds to, what
//! one round writes, that a write never waits for an answer or for the socket,
//! and what an answer applies when it does come back. It also covers what a
//! paste sends, what it ends, what a paste too big for one frame costs, the
//! picker that chooses how long the loop may sleep, and what one frame draws:
//! the mode it moves the viewer to, the hint bar it lists that mode's bindings
//! in, and what a pass must move before the loop draws again on its own. It also
//! covers coming back after the session replaces its own process image: which
//! endpoint file the wait takes and which it reads past, that the join names the
//! client record this terminal holds, and every way back that fails reporting
//! the death a broken connection already reports. It also covers a remote
//! viewer whose link broke: the pause each redial waits, that dialing again
//! moves what the viewer paints, what the drain of the stretch with no link
//! keeps and what it drops, and what a viewer that stopped dialing prints and
//! exits with.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::backend::TestBackend;

use koshi_core::command::{
    ClearSelectionArgs, CliExitCode, GridPos, Selection, SelectionKind, SetSelectionArgs,
    VisualCommand,
};
use koshi_core::geometry::{Direction, Point, Rect};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_core::key::{KeyChord, ModFlags};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseAnswer, MouseButton, MouseTracking, ScrollDirection};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::endpoint::{socket_addr, EndpointFile};
use koshi_ipc::frame::{FrameClient, FrameSession, FrameTab, PaintedFrame};
use koshi_ipc::protocol::{
    ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcResponse, PROTOCOL_VERSION,
};
use koshi_ipc::remote_servers::SavedServer;
use koshi_ipc::router::{router_endpoint_path, router_socket_addr, RouterRequest, RouterResponse};
use koshi_ipc::transport::{Listener, MAX_FRAME_LEN};
use koshi_layout::mode::LayoutMode;
use koshi_renderer::snapshot::{
    ClientSnapshot, MousePane, PaneKind, PaneSlot, SessionSnapshot, TabMeta, TabSnapshot,
};
use tempfile::TempDir;

use super::*;

/// The smallest frame a session can paint: one empty tab, no panes, the client
/// unlocked. [`classify`] reads the frame's variant and nothing inside it.
fn painted_frame() -> PaintedFrame {
    painted_frame_in(LockMode::Normal)
}

/// The same frame, reporting the client in `lock_mode`. Each call mints a new
/// session id and a new tab id.
fn painted_frame_in(lock_mode: LockMode) -> PaintedFrame {
    let tab = TabId::new();
    PaintedFrame {
        session: FrameSession {
            id: SessionId::new(),
            name: String::from("session"),
            active_tab: FrameTab {
                id: tab,
                name: String::from("tab"),
                slots: Vec::new(),
                effective_size: Size { cols: 80, rows: 24 },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
            },
            tabs: Vec::new(),
        },
        panes: Vec::new(),
        client: FrameClient {
            id: ClientId::new(),
            viewport: Size { cols: 80, rows: 24 },
            active_tab: tab,
            focused_pane: None,
            lock_mode,
            mouse_select: false,
        },
    }
}

/// One listing row for a session displayed as `name`.
fn session_row(name: &str) -> SessionRow {
    SessionRow {
        id: SessionId::new(),
        name: String::from(name),
    }
}

#[test]
fn no_running_session_leaves_nothing_to_attach_to() {
    let error = pick(&[], "").expect_err("an empty listing has no session");
    assert_eq!(error.to_string(), "no koshi session is running");
    assert_eq!(CliExitCode::from(&error), CliExitCode::SessionNotFound);
}

#[test]
fn one_running_session_is_the_answer_without_reading_a_line() {
    let rows = vec![session_row("solo")];
    assert_eq!(pick(&rows, "").expect("one row needs no picking"), 0);
}

#[test]
fn the_typed_number_picks_that_row() {
    let rows = vec![session_row("a"), session_row("b"), session_row("c")];
    assert_eq!(pick(&rows, "2").expect("2 is in range"), 1);
    assert_eq!(
        pick(&rows, "2\n").expect("the read line keeps its newline"),
        1
    );
}

#[test]
fn two_rows_carrying_one_session_id_are_told_apart_by_their_place() {
    // Two rows, one session id: the answer is the place, not the id.
    let shared = SessionId::new();
    let rows = vec![
        SessionRow {
            id: shared,
            name: String::from("desk web"),
        },
        SessionRow {
            id: shared,
            name: String::from("desk-ip web"),
        },
    ];

    assert_eq!(pick(&rows, "1").expect("1 is in range"), 0);
    assert_eq!(
        pick(&rows, "2").expect("2 is in range"),
        1,
        "the second row is reachable even though it shares the first row's id"
    );
}

#[test]
fn a_line_that_is_not_a_listed_number_is_refused() {
    let rows = vec![session_row("a"), session_row("b"), session_row("c")];
    for typed in ["0", "4", "x"] {
        let error = pick(&rows, typed).expect_err("the line names no listed row");
        assert_eq!(
            error.to_string(),
            format!(
                "invalid arguments: `{typed}` is not one of the listed sessions; \
                 expected a number 1 to 3"
            )
        );
        assert_eq!(CliExitCode::from(&error), CliExitCode::UsageOrConfig);
    }
}

/// A fresh directory to stand in for the runtime dir, under a short base so
/// the Unix socket path stays inside the OS path-length cap. Removed when the
/// test drops it.
fn test_runtime_dir() -> TempDir {
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    TempDir::new_in(base).expect("a temporary runtime directory")
}

/// A stand-in router that records the name of every request it is asked and
/// refuses each one, so a caller that does ask never waits for an answer.
///
/// It serves one connection and records each request before it refuses it, so
/// a caller that has its refusal is a caller whose request is already in the
/// returned list. That ordering is what lets a test read the list once its
/// caller has returned, without joining the thread.
fn recording_router(runtime_dir: &Path) -> Arc<Mutex<Vec<&'static str>>> {
    let addr = router_socket_addr(runtime_dir);
    let listener = Listener::bind(&addr).expect("bind the stand-in router");
    EndpointFile {
        socket: addr,
        token: ConnectionToken::generate(),
        pid: std::process::id(),
    }
    .write(&router_endpoint_path(runtime_dir))
    .expect("write the router endpoint file");

    let asked = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&asked);
    thread::spawn(move || {
        let Ok(mut connection) = listener.accept() else {
            return;
        };
        while let Ok(request) = connection.recv::<RouterRequest>() {
            recorded
                .lock()
                .expect("the list outlives every panic")
                .push(request.kind.name());
            let _ = connection.send(&RouterResponse {
                request_id: Some(request.request_id),
                result: RouterResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::BadToken,
                    message: String::from("the stand-in router refuses every request"),
                }),
            });
        }
    });
    asked
}

#[test]
fn attaching_by_id_skips_the_selector_lookup() {
    let runtime_dir = test_runtime_dir();
    let router = recording_router(runtime_dir.path());
    let session_id = SessionId::new();

    let error = attach_session(runtime_dir.path(), session_id)
        .expect_err("no endpoint file advertises that session");

    let asked = router
        .lock()
        .expect("the list outlives every panic")
        .clone();
    assert_eq!(
        asked.iter().filter(|name| **name == "AttachLookup").count(),
        0,
        "the router was asked {asked:?}"
    );
    let CliError::SessionNotFound { session } = error else {
        panic!("expected SessionNotFound, got {error:?}");
    };
    assert_eq!(session, session_id.to_string());
}

#[test]
fn the_detached_frame_ends_the_stream_cleanly() {
    assert_eq!(
        classify(&Ok(SessionEvent::Detached)),
        Some(Ending::Detached)
    );
}

#[test]
fn the_quit_frame_ends_the_stream_with_the_session() {
    assert_eq!(
        classify(&Ok(SessionEvent::Quit)),
        Some(Ending::SessionEnded)
    );
}

#[test]
fn the_switch_frame_ends_the_stream_with_the_session_to_join_next() {
    let session_id = SessionId::new();
    assert_eq!(
        classify(&Ok(SessionEvent::SwitchTo { session_id })),
        Some(Ending::Switch(session_id))
    );
}

#[test]
fn a_closed_socket_ends_the_stream_as_a_death() {
    assert_eq!(classify(&Err(IpcError::Disconnected)), Some(Ending::Died));
}

#[test]
fn a_frame_that_does_not_decode_ends_the_stream_as_a_death() {
    let frame = Err(IpcError::MalformedFrame {
        detail: "expected value".to_string(),
    });
    assert_eq!(classify(&frame), Some(Ending::Died));
}

#[test]
fn a_transport_failure_ends_the_stream_as_a_death() {
    let frame = Err(IpcError::Transport {
        detail: "connection reset".to_string(),
    });
    assert_eq!(classify(&frame), Some(Ending::Died));
}

#[test]
fn every_other_frame_keeps_the_stream_reading() {
    let tab_id = TabId::new();
    let frames = [
        SessionEvent::PaneCreated {
            pane_id: PaneId::new(),
            tab_id,
        },
        SessionEvent::PaneProcessExited {
            pane_id: PaneId::new(),
            exit_code: Some(0),
        },
        SessionEvent::PaneClosing {
            pane_id: PaneId::new(),
        },
        SessionEvent::PaneRemoved {
            pane_id: PaneId::new(),
            tab_id,
        },
        SessionEvent::PaneFocused {
            client_id: ClientId::new(),
            tab_id,
            pane_id: PaneId::new(),
            prior_pane: None,
        },
        SessionEvent::LayoutChanged { tab_id },
        SessionEvent::TabCreated { tab_id },
        SessionEvent::TabClosed { tab_id },
        SessionEvent::TabFocused {
            client_id: ClientId::new(),
            tab_id,
            prior_tab: TabId::new(),
        },
        SessionEvent::TabMoved {
            tab_id,
            old_index: 0,
            new_index: 1,
        },
        SessionEvent::Resync { dropped_count: 3 },
    ];
    for frame in frames {
        assert_eq!(classify(&Ok(frame.clone())), None, "{frame:?}");
    }
}

#[test]
fn a_painted_frame_keeps_the_stream_reading() {
    let frame = SessionEvent::Painted {
        frame: Box::new(painted_frame()),
    };
    assert_eq!(classify(&Ok(frame)), None);
}

#[test]
fn a_mouse_answer_keeps_the_stream_reading() {
    let frame = SessionEvent::MouseAnswer {
        request_id: 7,
        answers: vec![MouseAnswer::Resized {
            pane: PaneId::new(),
            side: Direction::Up,
            step: -1,
            applied: 3,
        }],
    };
    assert_eq!(classify(&Ok(frame)), None);
}

/// A session on this machine, which is the home every report test below uses
/// unless it names the remote one.
fn local_home() -> Home {
    Home::Local {
        runtime_dir: PathBuf::from("/tmp/koshi-test"),
    }
}

/// A session on another machine, reached through a saved server named `work`.
fn remote_home() -> Home {
    Home::Remote {
        server: ServerArg::Saved(SavedServer {
            name: Some("work".to_string()),
            address: "laptop.local:7654".to_string(),
            secret: ConnectionToken::generate(),
            fingerprint: "00".repeat(32),
            added_at: SystemTime::UNIX_EPOCH,
            last_used_at: None,
        }),
    }
}

#[test]
fn a_death_reports_the_cause_and_how_to_reattach() {
    let session_id = SessionId::new();
    let error = report(&local_home(), Ending::Died, session_id).expect_err("a death is an error");
    assert_eq!(
        error.to_string(),
        format!(
            "the session ended unexpectedly\n  \
             run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&error), CliExitCode::RuntimeAction);
}

#[test]
fn a_death_on_another_machine_reports_the_way_back_to_that_machine() {
    // The local way back would send the user to their own machine, where the
    // session never ran, so the message names the saved server instead.
    let session_id = SessionId::new();
    let error = report(&remote_home(), Ending::Died, session_id).expect_err("a death is an error");
    assert_eq!(
        error.to_string(),
        format!(
            "the session ended unexpectedly\n  \
             run `koshi attach --remote work` to see that server's sessions; \
             if session {session_id} is among them, reattach with \
             `koshi attach --remote work {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&error), CliExitCode::RuntimeAction);
}

#[test]
fn a_server_with_no_name_is_named_by_its_address_in_the_way_back() {
    let session_id = SessionId::new();
    let home = Home::Remote {
        server: ServerArg::New {
            address: "laptop.local:7654".to_string(),
        },
    };
    let error = report(&home, Ending::Died, session_id).expect_err("a death is an error");
    assert_eq!(
        error.to_string(),
        format!(
            "the session ended unexpectedly\n  \
             run `koshi attach --remote laptop.local:7654` to see that server's sessions; \
             if session {session_id} is among them, reattach with \
             `koshi attach --remote laptop.local:7654 {session_id}`"
        )
    );
}

#[test]
fn a_viewer_that_stopped_dialing_reports_the_cause_that_stopped_it_and_the_way_back() {
    let session_id = SessionId::new();
    let cause = CliError::Runtime {
        detail: "the token this server saved does not reach session 7".to_string(),
    };
    let error = report(
        &remote_home(),
        Ending::LinkLost(Box::new(cause)),
        session_id,
    )
    .expect_err("a lost link is an error");
    assert_eq!(
        error.to_string(),
        format!(
            "the token this server saved does not reach session 7\n  \
             the session continues without you\n  \
             run `koshi attach --remote work` to see that server's sessions; \
             if session {session_id} is among them, reattach with \
             `koshi attach --remote work {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&error), CliExitCode::RuntimeAction);
}

#[test]
fn two_lost_links_are_equal_only_when_their_causes_print_the_same_text() {
    let lost = |detail: &str| {
        Ending::LinkLost(Box::new(CliError::Runtime {
            detail: detail.to_string(),
        }))
    };

    assert_eq!(lost("connection refused"), lost("connection refused"));
    assert_ne!(lost("connection refused"), lost("connection timed out"));
    assert_ne!(lost("connection refused"), Ending::Died);
    assert_ne!(Ending::Died, lost("connection refused"));
}

#[test]
fn a_refused_dial_ends_the_redial_at_once_and_is_the_cause_it_stops_on() {
    let mut client = viewer();
    let mut screen = test_screen();
    let mut dials = 0_u32;
    let Err(cause) = redial_with(
        || {
            dials += 1;
            Err(DialError::Refused(CliError::Runtime {
                detail: "the server desk.local:7654 did not admit the connection".to_string(),
            }))
        },
        SessionId::new(),
        &mut client,
        &mut screen,
        None,
    ) else {
        panic!("a refused dial never joins");
    };
    assert_eq!(dials, 1);
    assert_eq!(
        cause.to_string(),
        "the server desk.local:7654 did not admit the connection"
    );
    assert_eq!(client.reconnecting, None);
}

#[test]
fn a_detach_and_a_session_end_both_succeed_and_name_no_session_to_join_next() {
    let session_id = SessionId::new();
    assert_eq!(
        report(&local_home(), Ending::Detached, session_id).expect("a detach is a success"),
        None
    );
    assert_eq!(
        report(&local_home(), Ending::SessionEnded, session_id)
            .expect("a session ending is a success"),
        None
    );
}

#[test]
fn a_switch_names_the_session_to_join_next() {
    let target = SessionId::new();
    assert_eq!(
        report(&local_home(), Ending::Switch(target), SessionId::new())
            .expect("a switch is a success"),
        Some(target)
    );
}

#[test]
fn the_restarting_frame_ends_the_stream_to_come_back_on_the_new_socket() {
    assert_eq!(
        classify(&Ok(SessionEvent::Restarting)),
        Some(Ending::Restarting)
    );
}

/// Advertise `session_id` at `token` and `pid`, the way a session server does
/// every time it binds, and hand back what was written.
fn advertise(runtime_dir: &Path, session_id: SessionId, token: &str, pid: u32) -> EndpointFile {
    let endpoint = EndpointFile {
        socket: socket_addr(runtime_dir, session_id),
        token: ConnectionToken::new(token),
        pid,
    };
    endpoint
        .write(&EndpointFile::path(runtime_dir, session_id))
        .expect("write the session endpoint file");
    endpoint
}

/// The token a test's client attached under, which the wait watches for a
/// change.
const OLD_TOKEN: &str = "the token this client attached under";

/// The token the image replacing the session mints when it binds again.
const NEW_TOKEN: &str = "the token the new image minted";

#[test]
fn the_wait_takes_the_endpoint_file_the_moment_it_names_another_token() {
    let runtime_dir = test_runtime_dir();
    let session_id = SessionId::new();
    let advertised = advertise(runtime_dir.path(), session_id, NEW_TOKEN, 4321);

    let deadline = Instant::now() + Duration::from_secs(10);
    assert_eq!(
        wait_for_new_endpoint(
            runtime_dir.path(),
            session_id,
            &ConnectionToken::new(OLD_TOKEN),
            deadline,
        ),
        Some(advertised)
    );
    assert!(
        Instant::now() < deadline,
        "the wait returned before its deadline"
    );
}

#[test]
fn the_wait_reads_the_endpoint_file_again_until_the_token_changes() {
    let runtime_dir = test_runtime_dir();
    let session_id = SessionId::new();
    advertise(runtime_dir.path(), session_id, OLD_TOKEN, 4321);

    let path = runtime_dir.path().to_path_buf();
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        advertise(&path, session_id, NEW_TOKEN, 4321)
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let found = wait_for_new_endpoint(
        runtime_dir.path(),
        session_id,
        &ConnectionToken::new(OLD_TOKEN),
        deadline,
    );
    let advertised = writer.join().expect("the writing thread finished");
    assert_eq!(found, Some(advertised));
    assert!(
        Instant::now() < deadline,
        "the wait returned before its deadline"
    );
}

#[test]
fn the_wait_ignores_an_endpoint_file_still_naming_the_token_this_client_attached_under() {
    let runtime_dir = test_runtime_dir();
    let session_id = SessionId::new();
    // A different process id under the same token. On Unix `execvp` keeps the
    // process id, so only the token tells the new image from the old one.
    advertise(runtime_dir.path(), session_id, OLD_TOKEN, 9999);

    assert_eq!(
        wait_for_new_endpoint(
            runtime_dir.path(),
            session_id,
            &ConnectionToken::new(OLD_TOKEN),
            Instant::now(),
        ),
        None
    );
}

#[test]
fn the_wait_ends_at_its_deadline_when_the_session_advertises_nothing() {
    let runtime_dir = test_runtime_dir();
    // No endpoint file at all, which is what the swap leaves while the
    // session's socket is down.
    assert_eq!(
        wait_for_new_endpoint(
            runtime_dir.path(),
            SessionId::new(),
            &ConnectionToken::new(OLD_TOKEN),
            Instant::now(),
        ),
        None
    );
}

/// A stand-in session that has just come back from replacing its own process
/// image: it binds `session_id`'s socket, advertises it under [`NEW_TOKEN`],
/// and serves one connection.
///
/// `attached` is how it answers the Attach: `Ok` is the client id it hands back,
/// `Err` is the sentence it refuses with. It records the Attach it read before
/// it answers, so a caller holding that answer is a caller whose request is
/// already in the returned slot.
fn restarted_session(
    runtime_dir: &Path,
    session_id: SessionId,
    attached: Result<ClientId, &'static str>,
) -> Arc<Mutex<Option<IpcRequestKind>>> {
    let addr = socket_addr(runtime_dir, session_id);
    let listener = Listener::bind(&addr).expect("bind the stand-in session");
    advertise(runtime_dir, session_id, NEW_TOKEN, std::process::id());

    let asked = Arc::new(Mutex::new(None));
    let recorded = Arc::clone(&asked);
    thread::spawn(move || {
        let Ok(mut connection) = listener.accept() else {
            return;
        };
        while let Ok(request) = connection.recv::<IpcRequest>() {
            let result = match &request.kind {
                IpcRequestKind::Hello { .. } => IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    version: String::from("0.0.0"),
                },
                IpcRequestKind::Attach { .. } => {
                    *recorded.lock().expect("the slot outlives every panic") =
                        Some(request.kind.clone());
                    match attached {
                        Ok(client_id) => IpcResult::Attached {
                            client_id,
                            session_id,
                            structure: AttachedSessionStructureSnapshot {
                                id: session_id,
                                name: String::from("session"),
                                tabs: Vec::new(),
                                panes: Vec::new(),
                            },
                            resume_token: None,
                        },
                        Err(message) => IpcResult::Error(IpcErrorPayload {
                            code: IpcErrorCode::BadToken,
                            message: String::from(message),
                        }),
                    }
                }
                _ => IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::UnsupportedKind,
                    message: String::from("the stand-in session serves only a join"),
                }),
            };
            let _ = connection.send(&IpcResponse {
                request_id: Some(request.request_id),
                result,
            });
        }
    });
    asked
}

#[test]
fn coming_back_after_a_restart_asks_for_the_client_record_this_terminal_holds() {
    let runtime_dir = test_runtime_dir();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let asked = restarted_session(runtime_dir.path(), session_id, Ok(client_id));
    let advertised = EndpointFile::read(&EndpointFile::path(runtime_dir.path(), session_id))
        .expect("the stand-in session advertised its socket");

    let (endpoint, connection) = rejoin(
        runtime_dir.path(),
        session_id,
        client_id,
        &ConnectionToken::new(OLD_TOKEN),
    )
    .expect("the stand-in session handed the client record back");
    drop(connection);

    assert_eq!(endpoint, advertised);
    assert_eq!(
        asked.lock().expect("the slot outlives every panic").clone(),
        Some(IpcRequestKind::Attach {
            viewport: viewport(),
            filter: EventFilterSpec::All,
            resume: Some(client_id),
            resume_token: None,
        }),
        "the restart path claims the client record by its id and presents no token"
    );
}

#[test]
fn a_restarted_session_that_refuses_the_join_leaves_nothing_to_come_back_to() {
    let runtime_dir = test_runtime_dir();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let _asked = restarted_session(
        runtime_dir.path(),
        session_id,
        Err("this session holds no such client"),
    );

    let attached = rejoin(
        runtime_dir.path(),
        session_id,
        client_id,
        &ConnectionToken::new(OLD_TOKEN),
    );
    assert_eq!(attached.map(|(endpoint, _)| endpoint), None);
}

#[test]
fn a_restarted_session_that_mints_a_new_client_leaves_nothing_to_come_back_to() {
    let runtime_dir = test_runtime_dir();
    let session_id = SessionId::new();
    let _asked = restarted_session(runtime_dir.path(), session_id, Ok(ClientId::new()));

    let attached = rejoin(
        runtime_dir.path(),
        session_id,
        ClientId::new(),
        &ConnectionToken::new(OLD_TOKEN),
    );
    assert_eq!(attached.map(|(endpoint, _)| endpoint), None);
}

#[test]
fn another_local_users_restarting_session_is_not_waited_for() {
    let runtime_dir = test_runtime_dir();
    let started = Instant::now();

    // The empty token is what a session another local user started is reached
    // with: this user reads no endpoint file for it, so there is none to watch.
    let attached = rejoin(
        runtime_dir.path(),
        SessionId::new(),
        ClientId::new(),
        &ConnectionToken::new(""),
    );

    assert_eq!(attached.map(|(endpoint, _)| endpoint), None);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "no wait was spent on a session this client cannot watch"
    );
}

#[test]
fn a_restart_this_client_cannot_come_back_from_reports_the_death_it_reports_today() {
    let session_id = SessionId::new();
    let error = report(&local_home(), Ending::Restarting, session_id)
        .expect_err("a restart with no way back is an error");
    assert_eq!(
        error.to_string(),
        format!(
            "the session ended unexpectedly\n  \
             run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&error), CliExitCode::RuntimeAction);
}

/// The terminal size every mouse fixture below is built at.
const MOUSE_VIEWPORT: Size = Size { cols: 80, rows: 24 };

/// A viewer on the stock settings — `scroll_lines` 3, wheel scrolls scrollback,
/// border resize on. Its subscription has no sender: frames reach an attached
/// client over the connection instead.
fn viewer() -> Client {
    let (_events_tx, events_rx) = mpsc::sync_channel(8);
    Client::new(
        ClientId::new(),
        MOUSE_VIEWPORT,
        events_rx,
        TerminalCleanupGuard::new(),
    )
}

/// One plain terminal pane: no highlight, no mouse mode, on the primary screen.
fn plain_pane(id: PaneId) -> MousePane {
    MousePane {
        id,
        view_top_row: 0,
        mouse_tracking: MouseTracking::Off,
        alt_scroll: false,
        on_alt_screen: false,
        has_selection: false,
    }
}

/// A frame of `panes`, laid out as full-width horizontal bands between the
/// tabline (row 0) and the hint bar (last row), with the first pane focused.
///
/// Two panes in an 80x24 viewport gives band rows 1..=11 and 12..=22, each with
/// a one-cell border ring, so the second band's top row is the divider the two
/// panes share.
fn mouse_frame(panes: &[MousePane]) -> MouseFrame {
    let tab_id = TabId::new();
    let band = (MOUSE_VIEWPORT.rows - 2) / u16::try_from(panes.len()).expect("few panes");
    let layout_solved: Vec<PaneSlot> = panes
        .iter()
        .enumerate()
        .map(|(index, pane)| {
            let top = 1 + band * u16::try_from(index).expect("few panes");
            PaneSlot {
                pane_id: pane.id,
                rect: Rect::new(
                    Point { x: 0, y: top },
                    Size {
                        cols: MOUSE_VIEWPORT.cols,
                        rows: band,
                    },
                ),
                inner_rect: Some(Rect::new(
                    Point { x: 1, y: top + 1 },
                    Size {
                        cols: MOUSE_VIEWPORT.cols - 2,
                        rows: band - 2,
                    },
                )),
                kind: PaneKind::Terminal,
                visible: true,
                suppressed: false,
                dead: false,
            }
        })
        .collect();
    MouseFrame {
        session: SessionSnapshot {
            id: SessionId::new(),
            name: String::from("fixture"),
            active_tab: TabSnapshot {
                id: tab_id,
                name: String::from("one"),
                layout_solved,
                effective_size: MOUSE_VIEWPORT,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                all_suppressed: false,
            },
            tabs_metadata: vec![TabMeta {
                id: tab_id,
                name: String::from("one"),
                index: 0,
                active: true,
            }],
        },
        panes: panes.to_vec(),
        client: ClientSnapshot {
            id: ClientId::new(),
            viewport: MOUSE_VIEWPORT,
            active_tab: tab_id,
            focused_pane: panes.first().map(|pane| pane.id),
            lock_mode: LockMode::Normal,
            mouse_select: false,
        },
    }
}

/// A cell inside the content of the `index`-th pane in a fixture frame.
fn content_cell(frame: &MouseFrame, index: usize) -> Point {
    let inner = frame.session.active_tab.layout_solved[index]
        .inner_rect
        .expect("a visible pane");
    Point {
        x: inner.origin.x + 1,
        y: inner.origin.y + 1,
    }
}

/// The row the two bands of a two-pane fixture frame share: the second pane's
/// top border.
fn divider(frame: &MouseFrame) -> Point {
    Point {
        x: 10,
        y: frame.session.active_tab.layout_solved[1].rect.origin.y,
    }
}

/// A wheel tick at `at`.
fn wheel(direction: ScrollDirection, at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Scroll(direction),
        at,
        mods: ModFlags::NONE,
    }
}

/// A left press at `at`.
fn press(at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Press(MouseButton::Left),
        at,
        mods: ModFlags::NONE,
    }
}

/// A left drag to `at`.
fn drag(at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Drag(MouseButton::Left),
        at,
        mods: ModFlags::NONE,
    }
}

/// A left release at `at`.
fn release(at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Release(MouseButton::Left),
        at,
        mods: ModFlags::NONE,
    }
}

/// One highlight change for `pane`, ending at line `row`.
fn selection(pane: PaneId, row: u64) -> MouseAction {
    MouseAction::Command(Command::Visual(VisualCommand::SetSelection(
        SetSelectionArgs {
            pane,
            selection: Selection {
                kind: SelectionKind::Character,
                anchor: GridPos { row: 0, col: 0 },
                cursor: GridPos { row, col: 4 },
            },
        },
    )))
}

/// One press handed to `pane`'s program, at column `x`.
fn forwarded(pane: PaneId, x: u16) -> MouseAction {
    MouseAction::Forward {
        pane,
        mouse: press(Point { x, y: 5 }),
    }
}

/// A live control socket with the two ends a test needs: the client end wrapped
/// in the shape the loop writes through, and the session end the requests are
/// read back off.
///
/// The accept runs on its own thread, since the connect and the accept must
/// both be live for the connection to open.
struct Wire {
    /// The reading half of the client end. Held, not read: this is what the
    /// frame-reader thread owns in the running loop.
    _reader: FrameReader,
    /// The session end: every request the uplink writes is read from here.
    session: Connection,
    /// The client end, numbered from [`FIRST_LOOP_REQUEST_ID`] exactly as the
    /// loop numbers it.
    uplink: Uplink,
}

fn wire() -> Wire {
    // The address is a socket-file path on Unix and a bare pipe name on
    // Windows, so the directory goes unused there. `/tmp` keeps the path short
    // enough for the platform's socket-path limit; the session id makes it
    // unique, so tests running side by side never share one.
    #[cfg(unix)]
    let base = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let base = std::env::temp_dir();
    let addr = socket_addr(&base, SessionId::new());
    let listener = Listener::bind(&addr).expect("bind the control socket");
    let accepting = thread::spawn(move || listener.accept().expect("accept the connection"));
    let connection = Connection::connect(&addr).expect("connect to the control socket");
    let session = accepting.join().expect("the accepting thread finished");
    let (reader, writer) = connection.split();
    Wire {
        _reader: reader,
        session,
        uplink: Uplink {
            requests: spawn_uplink_writer(writer),
            registry: ActionRegistry::new(),
            next_request_id: FIRST_LOOP_REQUEST_ID,
        },
    }
}

#[test]
fn three_ticks_over_one_pane_fold_to_one_scroll_of_the_summed_lines() {
    let pane = PaneId::new();
    let ticks = vec![
        MouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        },
        MouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        },
        MouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        },
    ];

    assert_eq!(
        coalesce(ticks),
        vec![MouseAction::Scroll {
            pane,
            up: true,
            lines: 9,
        }]
    );
}

#[test]
fn ticks_over_two_panes_stay_two_scrolls() {
    let first = PaneId::new();
    let second = PaneId::new();
    let ticks = vec![
        MouseAction::Scroll {
            pane: first,
            up: true,
            lines: 3,
        },
        MouseAction::Scroll {
            pane: second,
            up: true,
            lines: 3,
        },
    ];

    assert_eq!(coalesce(ticks.clone()), ticks);
}

#[test]
fn ticks_in_opposite_directions_stay_two_scrolls() {
    let pane = PaneId::new();
    let ticks = vec![
        MouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        },
        MouseAction::Scroll {
            pane,
            up: false,
            lines: 3,
        },
    ];

    assert_eq!(coalesce(ticks.clone()), ticks);
}

#[test]
fn two_alternate_scroll_runs_over_one_pane_sum_their_arrows() {
    let pane = PaneId::new();
    let runs = vec![
        MouseAction::AltScrollArrows {
            pane,
            up: false,
            count: 3,
        },
        MouseAction::AltScrollArrows {
            pane,
            up: false,
            count: 5,
        },
    ];

    assert_eq!(
        coalesce(runs),
        vec![MouseAction::AltScrollArrows {
            pane,
            up: false,
            count: 8,
        }]
    );
}

#[test]
fn two_highlight_changes_for_one_pane_keep_the_newer() {
    let pane = PaneId::new();

    assert_eq!(
        coalesce(vec![selection(pane, 12), selection(pane, 40)]),
        vec![selection(pane, 40)]
    );
}

#[test]
fn two_border_moves_for_one_pane_and_side_keep_the_newer_and_do_not_sum() {
    let pane = PaneId::new();
    let moves = vec![
        MouseAction::Resize {
            pane,
            side: Direction::Up,
            step: -1,
            count: 2,
        },
        MouseAction::Resize {
            pane,
            side: Direction::Up,
            step: -1,
            count: 5,
        },
    ];

    assert_eq!(
        coalesce(moves),
        vec![MouseAction::Resize {
            pane,
            side: Direction::Up,
            step: -1,
            count: 5,
        }],
        "each step is measured from the same anchor, so the newest is the whole move"
    );
}

#[test]
fn border_moves_on_two_sides_of_one_pane_stay_two_moves() {
    let pane = PaneId::new();
    let moves = vec![
        MouseAction::Resize {
            pane,
            side: Direction::Up,
            step: -1,
            count: 2,
        },
        MouseAction::Resize {
            pane,
            side: Direction::Left,
            step: 1,
            count: 4,
        },
    ];

    assert_eq!(coalesce(moves.clone()), moves);
}

#[test]
fn two_forwards_are_never_folded() {
    let pane = PaneId::new();
    let reports = vec![forwarded(pane, 4), forwarded(pane, 4)];

    assert_eq!(coalesce(reports.clone()), reports);
}

#[test]
fn a_forward_between_two_scrolls_keeps_all_three() {
    let pane = PaneId::new();
    let pile = vec![
        MouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        },
        forwarded(pane, 4),
        MouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        },
    ];

    assert_eq!(
        coalesce(pile.clone()),
        pile,
        "only neighbours fold, so the forward keeps the two scrolls apart"
    );
}

#[test]
fn an_empty_round_takes_no_request_id_and_writes_nothing() {
    let mut wire = wire();

    assert_eq!(send_round(&mut wire.uplink, Vec::new()), None);

    // The next request written is the first thing on the wire, and it carries
    // the id the empty round would have taken.
    let sentinel = wire.uplink.send(IpcRequestKind::Discovery);
    assert_eq!(sentinel, FIRST_LOOP_REQUEST_ID);
    let request: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID,
            kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn a_round_goes_out_as_one_request_holding_its_actions_in_order() {
    let pane = PaneId::new();
    let mut wire = wire();
    let round = vec![
        MouseAction::Scroll {
            pane,
            up: true,
            lines: 9,
        },
        forwarded(pane, 4),
        MouseAction::Resize {
            pane,
            side: Direction::Up,
            step: -1,
            count: 5,
        },
    ];

    assert_eq!(
        send_round(&mut wire.uplink, round),
        Some(FIRST_LOOP_REQUEST_ID)
    );

    let request: IpcRequest = wire.session.recv().expect("read the round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID,
            kind: IpcRequestKind::Mouse(vec![
                WireMouseAction::Scroll {
                    pane,
                    up: true,
                    lines: 9,
                },
                WireMouseAction::Forward {
                    pane,
                    mouse: press(Point { x: 4, y: 5 }),
                },
                WireMouseAction::Resize {
                    pane,
                    side: Direction::Up,
                    step: -1,
                    count: 5,
                },
            ]),
        }
    );

    // Nothing follows it: the whole round was one request.
    let sentinel = wire.uplink.send(IpcRequestKind::Discovery);
    let next: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        next,
        IpcRequest {
            request_id: sentinel,
            kind: IpcRequestKind::Discovery,
        }
    );
}

/// How many full rounds the writer-behind test queues while the session end
/// reads nothing.
///
/// Every round holds [`MAX_PENDING_MOUSE`] forwarded reports, which is about
/// 31 kB of frame, so 128 rounds come to about 3.9 MB — past every platform's
/// socket buffer, which is at most a few hundred kilobytes — and the writer
/// thread is therefore stuck inside a write for all but the first few.
const ROUNDS_WHILE_THE_WRITER_IS_STUCK: u64 = 128;

/// How long the test waits for the queueing to finish before it calls the loop
/// stalled. A watchdog, not a deadline the code observes: the queueing takes
/// milliseconds, and a write done on the queueing thread would never finish at
/// all.
const QUEUEING_WATCHDOG: Duration = Duration::from_secs(60);

/// One round of forwarded reports, each at its own column so the round read
/// back off the wire can be checked report for report.
fn full_round_of_reports(pane: PaneId) -> Vec<MouseAction> {
    (0..MAX_PENDING_MOUSE)
        .map(|index| forwarded(pane, u16::try_from(index).expect("the cap fits a u16")))
        .collect()
}

/// The same round in its wire spelling.
fn full_round_on_the_wire(pane: PaneId) -> Vec<WireMouseAction> {
    (0..MAX_PENDING_MOUSE)
        .map(|index| WireMouseAction::Forward {
            pane,
            mouse: press(Point {
                x: u16::try_from(index).expect("the cap fits a u16"),
                y: 5,
            }),
        })
        .collect()
}

#[test]
fn queueing_a_round_returns_while_a_full_socket_holds_the_writer() {
    let pane = PaneId::new();
    let Wire {
        _reader,
        mut session,
        mut uplink,
    } = wire();
    let (done_tx, done_rx) = mpsc::channel();

    // The session end reads nothing until every round is queued, so the socket
    // fills and the writer thread stops inside a write. The rounds are queued
    // on their own thread, which stands in for the loop: a write done there
    // would stop with the socket and this send would never return.
    let queueing = thread::spawn(move || {
        for round in 0..ROUNDS_WHILE_THE_WRITER_IS_STUCK {
            assert_eq!(
                send_round(&mut uplink, full_round_of_reports(pane)),
                Some(FIRST_LOOP_REQUEST_ID + round),
            );
        }
        done_tx.send(()).expect("the test is still waiting");
    });
    done_rx
        .recv_timeout(QUEUEING_WATCHDOG)
        .expect("every round was queued while the writer was stuck on the full socket");

    // Reading drains the socket and lets the writer run out. Every round
    // arrives whole and in the order it was queued: the backed-up queue drops
    // no forwarded report, folds none into another, and reorders none.
    for round in 0..ROUNDS_WHILE_THE_WRITER_IS_STUCK {
        let request: IpcRequest = session.recv().expect("read the round");
        assert_eq!(
            request,
            IpcRequest {
                request_id: FIRST_LOOP_REQUEST_ID + round,
                kind: IpcRequestKind::Mouse(full_round_on_the_wire(pane)),
            }
        );
    }
    queueing.join().expect("the queueing thread finished");
}

/// A viewer holding a border drag three cells past the divider it grabbed, and
/// the frame it decided against. The drag is what a matching `Resized` moves
/// and a stale one leaves alone.
fn dragging_a_border() -> (Client, MouseFrame, Point) {
    let frame = mouse_frame(&[plain_pane(PaneId::new()), plain_pane(PaneId::new())]);
    let grabbed = divider(&frame);
    let to = Point {
        y: grabbed.y + 3,
        ..grabbed
    };
    let mut client = viewer();
    let now = Instant::now();

    client.handle_mouse(press(grabbed), &frame, now);
    assert_eq!(
        client.handle_mouse(drag(to), &frame, now),
        vec![MouseAction::Resize {
            pane: frame.panes[1].id,
            side: Direction::Up,
            step: -1,
            count: 3,
        }],
        "three cells of travel away from the grabbed top border"
    );
    (client, frame, to)
}

/// The session's answer to a move of the top border [`dragging_a_border`]
/// grabbed on `pane`, `applied` cells of it accepted.
fn top_border_moved(pane: PaneId, applied: u16) -> MouseAnswer {
    MouseAnswer::Resized {
        pane,
        side: Direction::Up,
        step: -1,
        applied,
    }
}

/// The record of a move of `pane`'s top border, `cells` cells inward, written in
/// round `request_id` and not yet answered.
fn top_border_move_out(request_id: u64, pane: PaneId, cells: u16) -> SentBorderMove {
    SentBorderMove {
        request_id,
        pane,
        side: Direction::Up,
        cells: -i32::from(cells),
    }
}

#[test]
fn an_answer_moves_the_drag_anchor_whatever_round_it_came_back_in() {
    let (mut client, frame, to) = dragging_a_border();
    let mut sent = Vec::new();
    let mut pending = Vec::new();

    apply_answer(
        &mut client,
        &frame,
        &mut sent,
        8,
        vec![top_border_moved(frame.panes[1].id, 3)],
        &mut pending,
    );

    assert_eq!(sent, Vec::new());
    assert_eq!(pending, Vec::new());
    assert_eq!(
        client.handle_mouse(drag(to), &frame, Instant::now()),
        Vec::new(),
        "the answer names its own border, so the anchor moved to the pointer"
    );
}

#[test]
fn a_resized_forgets_only_the_border_move_its_own_round_wrote() {
    let (mut client, frame, _to) = dragging_a_border();
    let pane = frame.panes[1].id;
    let mut sent = vec![
        top_border_move_out(7, pane, 3),
        top_border_move_out(8, pane, 1),
    ];
    let mut pending = Vec::new();

    apply_answer(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![top_border_moved(pane, 3)],
        &mut pending,
    );

    assert_eq!(
        sent,
        vec![top_border_move_out(8, pane, 1)],
        "round 8's move is still on the wire"
    );
    assert_eq!(asked_for(&sent, pane, Direction::Up), -1);
}

#[test]
fn an_empty_answer_changes_nothing() {
    let (mut client, frame, to) = dragging_a_border();
    let mut sent = vec![top_border_move_out(7, frame.panes[1].id, 3)];
    let mut pending = Vec::new();

    apply_answer(&mut client, &frame, &mut sent, 7, Vec::new(), &mut pending);

    assert_eq!(
        sent,
        vec![top_border_move_out(7, frame.panes[1].id, 3)],
        "a round that reported no border move forgets none"
    );
    assert_eq!(pending, Vec::new());
    assert_eq!(
        client.handle_mouse(drag(to), &frame, Instant::now()),
        vec![MouseAction::Resize {
            pane: frame.panes[1].id,
            side: Direction::Up,
            step: -1,
            count: 3,
        }],
        "nothing was reported, so the anchor stayed where it was"
    );
}

#[test]
fn a_tick_reaches_the_wire_at_once_with_rounds_already_out() {
    let pane = PaneId::new();
    let frame = mouse_frame(&[plain_pane(pane)]);
    let at = content_cell(&frame, 0);
    let mut client = viewer();
    let mut wire = wire();
    let mut sent = Vec::new();
    let mut pending = Vec::new();

    // Four ticks, one per pass of the loop, none of them answered. Every pass
    // writes what it decided, so four rounds are on the wire unanswered.
    for round in 0..4u64 {
        handle_mouse_event(
            &mut client,
            &frame,
            wheel(ScrollDirection::Up, at),
            &mut pending,
        );
        flush_round(&mut wire.uplink, &mut sent, &mut pending);
        assert_eq!(pending, Vec::new(), "the pass that decided it wrote it");
        let request: IpcRequest = wire.session.recv().expect("read the round");
        assert_eq!(
            request,
            IpcRequest {
                request_id: FIRST_LOOP_REQUEST_ID + round,
                kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                    pane,
                    up: true,
                    lines: 3,
                }]),
            }
        );
    }

    // A fifth tick with those four still unanswered. It is written before the
    // sentinel that follows it, so it is the first request read off the wire.
    handle_mouse_event(
        &mut client,
        &frame,
        wheel(ScrollDirection::Up, at),
        &mut pending,
    );
    flush_round(&mut wire.uplink, &mut sent, &mut pending);
    let sentinel = wire.uplink.send(IpcRequestKind::Discovery);

    let request: IpcRequest = wire.session.recv().expect("read the fifth round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID + 4,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane,
                up: true,
                lines: 3,
            }]),
        },
        "four unanswered rounds held the fifth tick back by nothing"
    );
    let next: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        next,
        IpcRequest {
            request_id: sentinel,
            kind: IpcRequestKind::Discovery,
        }
    );
    assert_eq!(sent, Vec::new(), "no border move was written");
}

#[test]
fn ten_ticks_in_one_pass_leave_as_one_scroll_of_the_summed_lines() {
    let pane = PaneId::new();
    let frame = mouse_frame(&[plain_pane(pane)]);
    let at = content_cell(&frame, 0);
    let mut client = viewer();
    let mut wire = wire();
    let mut sent = Vec::new();
    let mut pending = Vec::new();

    // A burst of ten ticks read out of the channel as one batch, so all ten are
    // decided before the pass writes. No clock is read anywhere here.
    for _ in 0..10 {
        handle_mouse_event(
            &mut client,
            &frame,
            wheel(ScrollDirection::Up, at),
            &mut pending,
        );
    }
    assert_eq!(
        pending,
        vec![
            MouseAction::Scroll {
                pane,
                up: true,
                lines: 3,
            };
            10
        ],
        "one three-line scroll per tick, none written yet"
    );

    flush_round(&mut wire.uplink, &mut sent, &mut pending);

    assert_eq!(pending, Vec::new());
    let request: IpcRequest = wire.session.recv().expect("read the folded burst");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane,
                up: true,
                lines: 30,
            }]),
        },
        "ten ticks of three lines each, as one scroll of thirty"
    );

    // Nothing follows it: the whole burst was one request.
    let sentinel = wire.uplink.send(IpcRequestKind::Discovery);
    let next: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        next,
        IpcRequest {
            request_id: sentinel,
            kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn a_run_of_drag_moves_in_one_pass_leaves_as_the_newest_highlight() {
    let pane = PaneId::new();
    let frame = mouse_frame(&[plain_pane(pane)]);
    let mut client = viewer();
    let mut wire = wire();
    let mut sent = Vec::new();
    let mut pending = Vec::new();

    // The press arrives in a pass of its own and goes out as its own round, so
    // the drag moves below are the only thing the next pass holds.
    handle_mouse_event(
        &mut client,
        &frame,
        press(content_cell(&frame, 0)),
        &mut pending,
    );
    flush_round(&mut wire.uplink, &mut sent, &mut pending);
    let request: IpcRequest = wire.session.recv().expect("read the press");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Command(Box::new(Command::Visual(
                VisualCommand::ClearSelection(ClearSelectionArgs { pane })
            )))]),
        }
    );

    // Three drag moves arriving together in the next pass. The pane's content
    // starts at screen cell (1, 2) and shows line 0 on its top row, so the
    // pointer at (20, 12) names line 10, column 19.
    for at in [
        Point { x: 5, y: 5 },
        Point { x: 9, y: 8 },
        Point { x: 20, y: 12 },
    ] {
        handle_mouse_event(&mut client, &frame, drag(at), &mut pending);
    }
    assert_eq!(
        pending,
        [
            GridPos { row: 3, col: 4 },
            GridPos { row: 6, col: 8 },
            GridPos { row: 10, col: 19 },
        ]
        .map(
            |cursor| MouseAction::Command(Command::Visual(VisualCommand::SetSelection(
                SetSelectionArgs {
                    pane,
                    selection: Selection {
                        kind: SelectionKind::Character,
                        anchor: GridPos { row: 1, col: 1 },
                        cursor,
                    },
                }
            )))
        )
        .to_vec(),
        "one whole highlight per move, none written yet"
    );

    flush_round(&mut wire.uplink, &mut sent, &mut pending);

    assert_eq!(pending, Vec::new());
    let request: IpcRequest = wire.session.recv().expect("read the folded run");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID + 1,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Command(Box::new(
                Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
                    pane,
                    selection: Selection {
                        kind: SelectionKind::Character,
                        anchor: GridPos { row: 1, col: 1 },
                        cursor: GridPos { row: 10, col: 19 },
                    },
                }))
            ))]),
        },
        "the anchor is the press and the cursor is the last move, with nothing of the two before it"
    );

    // Nothing follows it: the whole run was one request.
    let sentinel = wire.uplink.send(IpcRequestKind::Discovery);
    let next: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        next,
        IpcRequest {
            request_id: sentinel,
            kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn every_report_in_one_pass_leaves_as_its_own_forward_in_order() {
    let plain = PaneId::new();
    let watched = PaneId::new();
    let frame = mouse_frame(&[
        plain_pane(plain),
        MousePane {
            mouse_tracking: MouseTracking::Normal,
            ..plain_pane(watched)
        },
    ]);
    let over_watched = content_cell(&frame, 1);
    let mut client = viewer();
    let mut wire = wire();
    let mut sent = Vec::new();
    let mut pending = Vec::new();

    // Five ticks on the pane whose program reads the mouse, arriving together in
    // one pass. The first two are the same tick twice: the pair a fold would
    // join.
    let ticks = [
        wheel(ScrollDirection::Up, over_watched),
        wheel(ScrollDirection::Up, over_watched),
        wheel(ScrollDirection::Down, over_watched),
        wheel(
            ScrollDirection::Up,
            Point {
                x: over_watched.x + 1,
                ..over_watched
            },
        ),
        wheel(
            ScrollDirection::Up,
            Point {
                y: over_watched.y + 1,
                ..over_watched
            },
        ),
    ];
    for tick in ticks {
        handle_mouse_event(&mut client, &frame, tick, &mut pending);
    }

    let reports: Vec<MouseAction> = ticks
        .iter()
        .map(|&mouse| MouseAction::Forward {
            pane: watched,
            mouse,
        })
        .collect();
    assert_eq!(pending, reports, "one report per tick, none written yet");

    flush_round(&mut wire.uplink, &mut sent, &mut pending);

    assert_eq!(pending, Vec::new());
    let request: IpcRequest = wire.session.recv().expect("read the round of reports");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID,
            kind: IpcRequestKind::Mouse(
                ticks
                    .iter()
                    .map(|&mouse| WireMouseAction::Forward {
                        pane: watched,
                        mouse,
                    })
                    .collect(),
            ),
        },
        "five reports in, five reports out, in the order they happened"
    );

    // Nothing follows it: the five reports were one request.
    let sentinel = wire.uplink.send(IpcRequestKind::Discovery);
    let next: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        next,
        IpcRequest {
            request_id: sentinel,
            kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn the_pile_stops_at_the_cap_and_keeps_every_forwarded_report() {
    let plain = PaneId::new();
    let watched = PaneId::new();
    let frame = mouse_frame(&[
        plain_pane(plain),
        MousePane {
            mouse_tracking: MouseTracking::Normal,
            ..plain_pane(watched)
        },
    ]);
    let over_plain = content_cell(&frame, 0);
    let over_watched = content_cell(&frame, 1);
    let mut client = viewer();
    let mut pending = Vec::new();
    let mut reports = Vec::new();

    // One pass holding more ticks than the cap. Ticks in alternating directions
    // never fold, so each one adds an action. Every hundredth lands on the pane
    // whose program reads the mouse, at its own column, and goes up as a report
    // instead of a scroll.
    for tick in 0..MAX_PENDING_MOUSE + 300 {
        let direction = if tick % 2 == 0 {
            ScrollDirection::Up
        } else {
            ScrollDirection::Down
        };
        if tick % 100 == 0 {
            let at = Point {
                x: over_watched.x + u16::try_from(tick / 100).expect("a handful of reports"),
                y: over_watched.y,
            };
            let mouse = wheel(direction, at);
            handle_mouse_event(&mut client, &frame, mouse, &mut pending);
            reports.push(MouseAction::Forward {
                pane: watched,
                mouse,
            });
        } else {
            handle_mouse_event(
                &mut client,
                &frame,
                wheel(direction, over_plain),
                &mut pending,
            );
        }
    }

    assert_eq!(
        pending.len(),
        MAX_PENDING_MOUSE,
        "the pile stopped at the cap"
    );
    assert_eq!(reports.len(), 6, "six ticks landed on the watched pane");
    let kept: Vec<MouseAction> = pending
        .iter()
        .filter(|action| reports.contains(action))
        .cloned()
        .collect();
    assert_eq!(
        kept, reports,
        "every forwarded report is still in the pile, in the order it happened"
    );
}

#[test]
fn a_border_move_decided_before_the_answer_asks_only_for_what_the_answer_left() {
    let (mut client, frame, to) = dragging_a_border();
    let further = Point { y: to.y + 1, ..to };
    let mut wire = wire();
    let mut sent = vec![top_border_move_out(7, frame.panes[1].id, 3)];
    let mut pending = Vec::new();

    // One more cell of travel while round 7 is out. It is measured from the
    // anchor round 7 started at, so it names all four cells.
    handle_mouse_event(&mut client, &frame, drag(further), &mut pending);
    assert_eq!(
        pending,
        vec![MouseAction::Resize {
            pane: frame.panes[1].id,
            side: Direction::Up,
            step: -1,
            count: 4,
        }]
    );

    apply_answer(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![top_border_moved(frame.panes[1].id, 3)],
        &mut pending,
    );
    assert_eq!(sent, Vec::new(), "round 7 is answered and forgotten");
    flush_round(&mut wire.uplink, &mut sent, &mut pending);

    // Three of the four cells are already travelled, so the round that goes out
    // asks for the one the pointer is still ahead by.
    let request: IpcRequest = wire.session.recv().expect("read the next round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                pane: frame.panes[1].id,
                side: Direction::Up,
                step: -1,
                count: 1,
            }]),
        }
    );
}

#[test]
fn a_border_move_the_answer_covered_whole_leaves_nothing_to_send() {
    let (mut client, frame, to) = dragging_a_border();
    let further = Point { y: to.y + 1, ..to };
    let mut wire = wire();
    let mut sent = vec![top_border_move_out(7, frame.panes[1].id, 3)];
    let mut pending = Vec::new();

    // The pointer goes one cell further and comes straight back, so the newest
    // buffered move names exactly the three cells round 7 asked for.
    handle_mouse_event(&mut client, &frame, drag(further), &mut pending);
    handle_mouse_event(&mut client, &frame, drag(to), &mut pending);

    apply_answer(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![top_border_moved(frame.panes[1].id, 3)],
        &mut pending,
    );
    flush_round(&mut wire.uplink, &mut sent, &mut pending);

    assert_eq!(sent, Vec::new(), "nothing was left to ask for");
    assert_eq!(pending, Vec::new());
    // Nothing reached the wire: the sentinel is the first request read.
    let sentinel = wire.uplink.send(IpcRequestKind::Discovery);
    let request: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        request,
        IpcRequest {
            request_id: sentinel,
            kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn an_answer_that_lands_while_the_pointer_is_still_moves_the_drag_anchor() {
    let (mut client, frame, to) = dragging_a_border();
    let pane = frame.panes[1].id;
    let mut wire = wire();
    let mut sent = vec![top_border_move_out(7, pane, 3)];
    let mut pending = Vec::new();

    // A fourth cell of travel while round 7 is out, then round 7's answer: the
    // anchor takes the three cells the session moved and the one cell left over
    // goes out as the next round.
    handle_mouse_event(
        &mut client,
        &frame,
        drag(Point { y: to.y + 1, ..to }),
        &mut pending,
    );
    apply_answer(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![top_border_moved(frame.panes[1].id, 3)],
        &mut pending,
    );
    flush_round(&mut wire.uplink, &mut sent, &mut pending);

    let request: IpcRequest = wire.session.recv().expect("read the second round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                pane,
                side: Direction::Up,
                step: -1,
                count: 1,
            }]),
        }
    );

    // The user pauses: the second round is answered with no drag event between
    // the flush and the answer, so nothing but the answer can move the anchor.
    apply_answer(
        &mut client,
        &frame,
        &mut sent,
        FIRST_LOOP_REQUEST_ID,
        vec![top_border_moved(frame.panes[1].id, 1)],
        &mut pending,
    );
    assert_eq!(sent, Vec::new());
    assert_eq!(pending, Vec::new());

    // One more cell of travel past the border it moved: the anchor sits on the
    // border's real row, so one cell of pointer travel asks for one cell.
    handle_mouse_event(
        &mut client,
        &frame,
        drag(Point { y: to.y + 2, ..to }),
        &mut pending,
    );
    assert_eq!(
        pending,
        vec![MouseAction::Resize {
            pane,
            side: Direction::Up,
            step: -1,
            count: 1,
        }]
    );
}

#[test]
fn two_border_moves_in_one_round_each_land_on_their_own_border() {
    let (mut client, frame, to) = dragging_a_border();
    let grabbed = frame.panes[1].id;
    let other = frame.panes[0].id;
    let further = Point { y: to.y + 1, ..to };
    let mut sent = vec![
        top_border_move_out(7, grabbed, 3),
        SentBorderMove {
            request_id: 7,
            pane: other,
            side: Direction::Left,
            cells: 6,
        },
    ];
    // A move of another pane's left border, buffered before round 7 went out.
    let mut pending = vec![MouseAction::Resize {
        pane: other,
        side: Direction::Left,
        step: 1,
        count: 6,
    }];

    // One more cell of travel on the grabbed border, so the pile holds one move
    // per border.
    handle_mouse_event(&mut client, &frame, drag(further), &mut pending);

    apply_answer(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![
            top_border_moved(grabbed, 3),
            MouseAnswer::Resized {
                pane: other,
                side: Direction::Left,
                step: 1,
                applied: 1,
            },
        ],
        &mut pending,
    );

    assert_eq!(sent, Vec::new(), "both of round 7's moves are answered");
    assert_eq!(
        pending,
        vec![
            MouseAction::Resize {
                pane: other,
                side: Direction::Left,
                step: 1,
                count: 5,
            },
            MouseAction::Resize {
                pane: grabbed,
                side: Direction::Up,
                step: -1,
                count: 1,
            },
        ],
        "each buffered move lost only the cells its own border's answer took"
    );
    assert_eq!(
        client.handle_mouse(drag(further), &frame, Instant::now()),
        vec![MouseAction::Resize {
            pane: grabbed,
            side: Direction::Up,
            step: -1,
            count: 1,
        }],
        "the anchor took the three cells of the grabbed border's own answer, and no more"
    );
}

#[test]
fn two_moves_of_one_border_in_one_round_travel_the_newest_distance() {
    let pane = PaneId::new();
    let mut wire = wire();
    let mut sent = Vec::new();
    // A wheel tick between two moves of one border keeps them apart through the
    // fold. Both measure the whole travel from the same drag anchor: three cells
    // out, then five.
    let mut pending = vec![
        MouseAction::Resize {
            pane,
            side: Direction::Up,
            step: -1,
            count: 3,
        },
        MouseAction::Scroll {
            pane,
            up: true,
            lines: 3,
        },
        MouseAction::Resize {
            pane,
            side: Direction::Up,
            step: -1,
            count: 5,
        },
    ];

    flush_round(&mut wire.uplink, &mut sent, &mut pending);

    assert_eq!(pending, Vec::new());
    let request: IpcRequest = wire.session.recv().expect("read the round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID,
            kind: IpcRequestKind::Mouse(vec![
                WireMouseAction::Resize {
                    pane,
                    side: Direction::Up,
                    step: -1,
                    count: 3,
                },
                WireMouseAction::Scroll {
                    pane,
                    up: true,
                    lines: 3,
                },
                WireMouseAction::Resize {
                    pane,
                    side: Direction::Up,
                    step: -1,
                    count: 2,
                },
            ]),
        },
        "the session travels each move in turn, so the second asks for the two cells the first left"
    );
    assert_eq!(
        sent,
        vec![
            top_border_move_out(FIRST_LOOP_REQUEST_ID, pane, 3),
            top_border_move_out(FIRST_LOOP_REQUEST_ID, pane, 2),
        ],
        "five cells of border travel recorded for the round, not eight"
    );
}

#[test]
fn an_answer_for_a_border_drag_that_already_ended_changes_nothing() {
    let frame = mouse_frame(&[
        plain_pane(PaneId::new()),
        plain_pane(PaneId::new()),
        plain_pane(PaneId::new()),
    ]);
    let first = frame.panes[1].id;
    let second = frame.panes[2].id;
    let upper = Point {
        x: 10,
        y: frame.session.active_tab.layout_solved[1].rect.origin.y,
    };
    let lower = Point {
        x: 10,
        y: frame.session.active_tab.layout_solved[2].rect.origin.y,
    };
    let held = Point {
        y: lower.y + 1,
        ..lower
    };
    let mut client = viewer();
    let now = Instant::now();
    let mut sent = vec![top_border_move_out(7, first, 3)];
    let mut pending = Vec::new();

    // Round 7 asks for three cells of the upper divider.
    client.handle_mouse(press(upper), &frame, now);
    assert_eq!(
        client.handle_mouse(
            drag(Point {
                y: upper.y + 3,
                ..upper
            }),
            &frame,
            now
        ),
        vec![MouseAction::Resize {
            pane: first,
            side: Direction::Up,
            step: -1,
            count: 3,
        }]
    );

    // The user lets go and grabs the lower divider instead, all while round 7
    // is still out.
    assert_eq!(
        client.handle_mouse(
            release(Point {
                y: upper.y + 3,
                ..upper
            }),
            &frame,
            now
        ),
        Vec::new()
    );
    client.handle_mouse(press(lower), &frame, now);
    handle_mouse_event(&mut client, &frame, drag(held), &mut pending);
    assert_eq!(
        pending,
        vec![MouseAction::Resize {
            pane: second,
            side: Direction::Up,
            step: -1,
            count: 1,
        }]
    );

    apply_answer(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![top_border_moved(first, 3)],
        &mut pending,
    );

    assert_eq!(sent, Vec::new(), "round 7's move is answered and forgotten");
    assert_eq!(
        pending,
        vec![MouseAction::Resize {
            pane: second,
            side: Direction::Up,
            step: -1,
            count: 1,
        }],
        "the buffered move is for the other border, so the answer left it alone"
    );
    assert_eq!(
        client.handle_mouse(drag(held), &frame, Instant::now()),
        vec![MouseAction::Resize {
            pane: second,
            side: Direction::Up,
            step: -1,
            count: 1,
        }],
        "the new drag's anchor never moved, so the same pointer still asks for one cell"
    );
}

#[test]
fn border_moves_written_back_to_back_ask_only_for_the_cells_no_round_asked_yet() {
    let frame = mouse_frame(&[plain_pane(PaneId::new()), plain_pane(PaneId::new())]);
    let pane = frame.panes[1].id;
    let grabbed = divider(&frame);
    let mut client = viewer();
    let mut wire = wire();
    let mut sent = Vec::new();
    let mut pending = Vec::new();

    client.handle_mouse(press(grabbed), &frame, Instant::now());

    // The pointer walks three cells down, one cell per pass, with none of the
    // rounds answered. Each move names its whole distance from the drag anchor —
    // 1, then 2, then 3 — and each round asks for the one cell the rounds
    // already on the wire did not.
    for cell in 1..=3u16 {
        handle_mouse_event(
            &mut client,
            &frame,
            drag(Point {
                y: grabbed.y + cell,
                ..grabbed
            }),
            &mut pending,
        );
        assert_eq!(
            pending,
            vec![MouseAction::Resize {
                pane,
                side: Direction::Up,
                step: -1,
                count: cell,
            }],
            "the anchor has not moved, so the whole distance is named again"
        );
        flush_round(&mut wire.uplink, &mut sent, &mut pending);
        let request: IpcRequest = wire.session.recv().expect("read the round");
        assert_eq!(
            request,
            IpcRequest {
                request_id: FIRST_LOOP_REQUEST_ID + u64::from(cell) - 1,
                kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                    pane,
                    side: Direction::Up,
                    step: -1,
                    count: 1,
                }]),
            }
        );
    }
    assert_eq!(
        asked_for(&sent, pane, Direction::Up),
        -3,
        "three cells asked for over three rounds, one each"
    );

    // The three answers come back, each taking its own round's cell off.
    for round in 0..3u64 {
        apply_answer(
            &mut client,
            &frame,
            &mut sent,
            FIRST_LOOP_REQUEST_ID + round,
            vec![top_border_moved(pane, 1)],
            &mut pending,
        );
    }

    assert_eq!(sent, Vec::new());
    assert_eq!(
        client.handle_mouse(
            drag(Point {
                y: grabbed.y + 3,
                ..grabbed
            }),
            &frame,
            Instant::now()
        ),
        Vec::new(),
        "the anchor took all three cells, so the still pointer asks for nothing"
    );
}

#[test]
fn a_border_move_the_session_refused_stops_coming_off_the_next_one() {
    let (mut client, frame, to) = dragging_a_border();
    let pane = frame.panes[1].id;
    let mut wire = wire();
    let mut sent = vec![top_border_move_out(7, pane, 3)];
    let mut pending = Vec::new();

    // Round 7 asked for three cells and the session took none: the border is
    // against a wall. The anchor stays put and the record goes, so the pointer's
    // next move asks for its whole distance again.
    apply_answer(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![top_border_moved(pane, 0)],
        &mut pending,
    );
    assert_eq!(sent, Vec::new());

    handle_mouse_event(&mut client, &frame, drag(to), &mut pending);
    flush_round(&mut wire.uplink, &mut sent, &mut pending);

    let request: IpcRequest = wire.session.recv().expect("read the next round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID,
            kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                pane,
                side: Direction::Up,
                step: -1,
                count: 3,
            }]),
        },
        "none of the three cells moved, so all three are asked for again"
    );
}

#[test]
fn a_paste_goes_up_the_connection_as_the_text_the_terminal_delivered() {
    let mut client = viewer();
    let client_id = client.id();
    let mut wire = wire();

    handle_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::HostPaste {
            client_id,
            text: String::from("hello\nworld"),
        },
    );

    // The sentinel goes out behind the paste, so a paste that was never sent
    // reads back as the sentinel instead of leaving this test waiting.
    wire.uplink.send(IpcRequestKind::Discovery);
    let request: IpcRequest = wire.session.recv().expect("read the paste");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID,
            kind: IpcRequestKind::Paste {
                text: String::from("hello\nworld"),
            },
        },
        "the block crosses whole, line break and all"
    );
}

#[test]
fn a_paste_too_big_for_one_frame_is_the_only_thing_lost() {
    let mut client = viewer();
    let client_id = client.id();
    let mut wire = wire();

    handle_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::HostPaste {
            client_id,
            text: "a".repeat(MAX_FRAME_LEN as usize + 1),
        },
    );
    wire.uplink.send(IpcRequestKind::Discovery);

    // The read runs on its own thread: a writer that ended on the paste writes
    // nothing more, and this reports that as a failure instead of waiting for a
    // frame that never comes.
    let (read_tx, read_rx) = mpsc::channel();
    let mut session = wire.session;
    thread::spawn(move || {
        let _ = read_tx.send(session.recv::<IpcRequest>());
    });
    let request = read_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the uplink still writes after the refused paste")
        .expect("read the request behind the paste");

    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_LOOP_REQUEST_ID + 1,
            kind: IpcRequestKind::Discovery,
        },
        "the paste alone is dropped, not the connection"
    );
}

#[test]
fn a_paste_ends_this_viewers_selection_gesture() {
    let pane = PaneId::new();
    let frame = mouse_frame(&[plain_pane(pane)]);
    let mut client = viewer();
    let client_id = client.id();
    let mut wire = wire();
    let mut pending = Vec::new();

    // Press and drag: the gesture is under way, so the move names a highlight.
    handle_mouse_event(
        &mut client,
        &frame,
        press(content_cell(&frame, 0)),
        &mut pending,
    );
    handle_mouse_event(
        &mut client,
        &frame,
        drag(Point { x: 9, y: 8 }),
        &mut pending,
    );
    assert_eq!(
        pending.last().expect("the drag decided something"),
        &MouseAction::Command(Command::Visual(VisualCommand::SetSelection(
            SetSelectionArgs {
                pane,
                selection: Selection {
                    kind: SelectionKind::Character,
                    anchor: GridPos { row: 1, col: 1 },
                    cursor: GridPos { row: 6, col: 8 },
                },
            }
        )))
    );

    handle_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::HostPaste {
            client_id,
            text: String::from("hello"),
        },
    );

    // The same move again, with no gesture left to extend.
    pending.clear();
    handle_mouse_event(
        &mut client,
        &frame,
        drag(Point { x: 20, y: 12 }),
        &mut pending,
    );
    assert_eq!(pending, Vec::new());
}

#[test]
fn earliest_of_two_present_durations_is_the_smaller_either_order() {
    let short = Duration::from_millis(5);
    let long = Duration::from_millis(50);
    assert_eq!(earliest(Some(short), Some(long)), Some(short));
    assert_eq!(earliest(Some(long), Some(short)), Some(short));
}

#[test]
fn earliest_of_two_equal_durations_returns_that_duration() {
    let same = Duration::from_millis(10);
    assert_eq!(earliest(Some(same), Some(same)), Some(same));
}

#[test]
fn earliest_falls_back_to_whichever_single_side_is_present() {
    let only = Duration::from_millis(7);
    assert_eq!(earliest(Some(only), None), Some(only));
    assert_eq!(earliest(None, Some(only)), Some(only));
}

#[test]
fn earliest_of_two_absent_durations_is_none() {
    assert_eq!(earliest(None, None), None);
}

/// A screen drawing into memory at [`MOUSE_VIEWPORT`], 80 by 24 cells.
fn test_screen() -> Screen<TestBackend> {
    Screen::new(
        Terminal::new(TestBackend::new(MOUSE_VIEWPORT.cols, MOUSE_VIEWPORT.rows))
            .expect("build an in-memory terminal"),
    )
}

/// The hint bar of what the screen last drew: the bottom row, trailing blanks
/// removed.
fn hint_row(screen: &Screen<TestBackend>) -> String {
    let buffer = screen.terminal.backend().buffer();
    let width = buffer.area.width as usize;
    buffer
        .content()
        .chunks(width)
        .last()
        .expect("a drawn frame has at least one row")
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// The first chord of a multi-chord binding the built-in normal mode carries:
/// one press that opens a sequence and sends nothing.
fn sequence_opener(client: &Client) -> KeyChord {
    client
        .keymap_hints()
        .entries
        .iter()
        .find(|entry| entry.sequence.chords().len() > 1)
        .expect("the built-in normal mode binds at least one multi-chord sequence")
        .sequence
        .chords()[0]
}

/// The pointer moved to `at` with no button held.
fn moved(at: Point) -> MouseInput {
    MouseInput {
        kind: MouseKind::Motion,
        at,
        mods: ModFlags::NONE,
    }
}

#[test]
fn the_frame_that_locks_the_client_draws_the_locked_hint_bar() {
    // The first draw leaves the viewer in normal mode. The second is the frame
    // that locks it, and its own paint lists the locked bindings.
    let mut client = viewer();
    let mut screen = test_screen();

    screen.draw(&mut client, Box::new(painted_frame_in(LockMode::Normal)));
    let normal = hint_row(&screen);

    screen.draw(&mut client, Box::new(painted_frame_in(LockMode::Locked)));
    let locked = hint_row(&screen);

    assert_eq!(locked, " Ctrl +  l  Unlock  g  Mouse Select  q  Quit");
    assert_ne!(normal, locked);
}

#[test]
fn a_frame_moves_the_viewer_to_the_mode_it_reports() {
    let mut client = viewer();
    assert_eq!(client.lock_mode(), LockMode::Normal);

    adopt_frame(
        &mut client,
        &to_snapshot(&painted_frame_in(LockMode::Locked)),
    );

    assert_eq!(client.lock_mode(), LockMode::Locked);
}

#[test]
fn a_prefix_key_typed_after_a_frame_in_one_pass_still_draws_its_breadcrumb() {
    // One drained batch can carry a frame and a keypress together. The frame is
    // drawn first, and the key opens a sequence after it, so the end of the pass
    // has to draw again.
    let mut client = viewer();
    let mut screen = test_screen();
    let frame = painted_frame_in(LockMode::Normal);
    let tab = frame.client.active_tab;

    screen.draw(&mut client, Box::new(frame));
    let drawn = hint_row(&screen);

    let opener = sequence_opener(&client);
    assert_eq!(
        client.resolve_key(opener, Instant::now()),
        KeyOutcome::Pending
    );
    screen.refresh(&client, Some(tab));

    // The breadcrumb view opens with the chord that was pressed and ends with
    // the arrow that separates it from the chords continuing it.
    let breadcrumb = hint_row(&screen);
    assert_ne!(breadcrumb, drawn);
    assert!(
        breadcrumb.contains(" ▶ "),
        "the bar draws the open sequence: {breadcrumb}"
    );
}

#[test]
fn a_pointer_moved_after_a_frame_in_one_pass_still_draws_the_new_hover() {
    // The same batch order with a mouse move: the pane under the pointer is the
    // viewer's own, and the frame drawn before the move does not carry it.
    let mut client = viewer();
    let mut screen = test_screen();
    let painted = painted_frame_in(LockMode::Normal);
    let tab = painted.client.active_tab;
    let first = PaneId::new();
    let second = PaneId::new();
    let mouse = mouse_frame(&[plain_pane(first), plain_pane(second)]);
    let mut pending = Vec::new();

    screen.draw(&mut client, Box::new(painted));
    let hovered_before = ViewerPaint::read(&client, tab).chrome.hovered_pane;

    handle_mouse_event(
        &mut client,
        &mouse,
        moved(content_cell(&mouse, 1)),
        &mut pending,
    );
    let hovered_after = ViewerPaint::read(&client, tab).chrome.hovered_pane;
    screen.refresh(&client, Some(tab));

    assert_eq!(hovered_before, None);
    assert_eq!(hovered_after, Some(second));
    assert_eq!(
        screen.shown.as_ref().map(|shown| shown.chrome.hovered_pane),
        Some(Some(second)),
        "the screen records the hover it just drew"
    );
}

#[test]
fn a_pass_that_moved_nothing_draws_nothing() {
    let mut client = viewer();
    let mut screen = test_screen();
    let frame = painted_frame_in(LockMode::Normal);
    let tab = frame.client.active_tab;

    screen.draw(&mut client, Box::new(frame));
    let drawn = screen.terminal.backend().buffer().clone();
    let shown = screen.shown.clone();

    screen.refresh(&client, Some(tab));

    assert_eq!(*screen.terminal.backend().buffer(), drawn);
    assert_eq!(screen.shown, shown);
}

#[test]
fn a_screen_with_no_frame_yet_draws_nothing() {
    let client = viewer();
    let mut screen = test_screen();

    screen.refresh(&client, None);
    screen.refresh(&client, Some(TabId::new()));

    assert_eq!(screen.shown, None);
    assert_eq!(screen.last_painted, None);
}

#[test]
fn locking_the_client_moves_what_the_viewer_paints() {
    let mut client = viewer();
    let frame = painted_frame_in(LockMode::Locked);
    let tab = frame.client.active_tab;
    let before = ViewerPaint::read(&client, tab);

    adopt_frame(&mut client, &to_snapshot(&frame));

    let after = ViewerPaint::read(&client, tab);
    assert_eq!(before.mode, LockMode::Normal);
    assert_eq!(after.mode, LockMode::Locked);
    assert_ne!(after, before);
}

#[test]
fn opening_a_key_sequence_moves_what_the_viewer_paints() {
    // An opened sequence reaches no session, so no frame comes back. The hint
    // bar draws it as a breadcrumb, so `ViewerPaint` carries it.
    let mut client = viewer();
    let tab = TabId::new();
    let opener = sequence_opener(&client);
    let before = ViewerPaint::read(&client, tab);

    assert_eq!(
        client.resolve_key(opener, Instant::now()),
        KeyOutcome::Pending
    );

    let after = ViewerPaint::read(&client, tab);
    assert_eq!(before.pending, None);
    assert_eq!(after.pending, Some(KeySequence::from(opener)));
    assert_ne!(after, before);
}

#[test]
fn dialing_again_moves_what_the_viewer_paints() {
    // No frame arrives while the link is down, so the `RECONNECTING` tag is
    // drawn by the repaint a moved `ViewerPaint` fires.
    let mut client = viewer();
    let tab = TabId::new();
    let before = ViewerPaint::read(&client, tab);

    let dialing = Reconnecting {
        attempt: 1,
        retry_in_seconds: 5,
    };
    client.set_reconnecting(Some(dialing));

    let after = ViewerPaint::read(&client, tab);
    assert_eq!(before.chrome.reconnecting, None);
    assert_eq!(after.chrome.reconnecting, Some(dialing));
    assert_ne!(after, before);
}

#[test]
fn every_event_from_the_blackout_is_dropped_and_the_hangup_still_reported() {
    let client = viewer();
    let typed = sequence_opener(&client);
    let resized = Size {
        cols: 100,
        rows: 40,
    };
    let (incoming_tx, incoming_rx) = mpsc::channel();
    incoming_tx
        .send(Incoming::Input(Box::new(RuntimeEvent::KeyInput {
            client_id: client.id(),
            chord: typed,
        })))
        .expect("the loop's channel takes it");
    incoming_tx
        .send(Incoming::Input(Box::new(RuntimeEvent::Quit)))
        .expect("the loop's channel takes it");
    incoming_tx
        .send(Incoming::Input(Box::new(RuntimeEvent::Resize {
            client_id: client.id(),
            size: resized,
        })))
        .expect("the loop's channel takes it");
    incoming_tx
        .send(Incoming::Frame {
            connection: 0,
            frame: Ok(SessionEvent::Painted {
                frame: Box::new(painted_frame()),
            }),
        })
        .expect("the loop's channel takes it");

    let before = client.viewport();

    assert!(
        drop_input_from_the_blackout(&incoming_rx),
        "the terminal hung up while the link was down"
    );

    assert_eq!(
        client.viewport(),
        before,
        "the resize was dropped with the rest; the new link reads the size itself"
    );
    assert_ne!(before, resized, "the dropped resize named another size");
    assert_eq!(
        incoming_rx.try_recv().err(),
        Some(mpsc::TryRecvError::Empty),
        "the key, the resize and the stale frame were all taken off the channel"
    );
}

#[test]
fn a_new_connection_is_told_the_size_the_terminal_is_now() {
    let mut client = viewer();
    let (requests, sent) = mpsc::channel();
    let mut uplink = Uplink {
        requests,
        registry: ActionRegistry::new(),
        next_request_id: FIRST_LOOP_REQUEST_ID,
    };

    report_terminal_size(&mut client, &mut uplink);

    let request = sent.try_recv().expect("the size was reported");
    assert_eq!(request.request_id, FIRST_LOOP_REQUEST_ID);
    let IpcRequestKind::Resize { viewport } = request.kind else {
        panic!("expected a Resize, got {:?}", request.kind);
    };
    assert_eq!(
        viewport,
        client.viewport(),
        "the session is told the size the viewer holds"
    );
    assert_eq!(
        sent.try_recv().err(),
        Some(mpsc::TryRecvError::Empty),
        "the size is reported once"
    );
}

#[test]
fn the_redial_wait_doubles_to_eight_seconds_and_holds_there() {
    let first = FIRST_REDIAL_WAIT;
    let second = next_redial_wait(first);
    let third = next_redial_wait(second);
    let fourth = next_redial_wait(third);
    let fifth = next_redial_wait(fourth);

    assert_eq!(first, Duration::from_secs(1));
    assert_eq!(second, Duration::from_secs(2));
    assert_eq!(third, Duration::from_secs(4));
    assert_eq!(fourth, Duration::from_secs(8));
    assert_eq!(fifth, Duration::from_secs(8));
}

#[test]
fn a_pause_ending_on_or_past_the_window_is_not_taken() {
    // The first pause always fits, so the ladder always dials at least once.
    assert!(pause_fits(Duration::ZERO, FIRST_REDIAL_WAIT));

    // 111 + 8 lands at 119 and fits; 112 + 8 lands exactly on 120 and does not.
    assert!(pause_fits(Duration::from_secs(111), Duration::from_secs(8)));
    assert!(!pause_fits(
        Duration::from_secs(112),
        Duration::from_secs(8)
    ));

    // A pause that would end long past the window is refused too.
    assert!(!pause_fits(REDIAL_WINDOW, FIRST_REDIAL_WAIT));
}

#[test]
fn every_pause_the_ladder_hands_out_stays_inside_the_window() {
    // Walking the real ladder from the first pause, the loop stops before any
    // pause crosses 120 seconds, and it stops after taking at least one.
    let mut elapsed = Duration::ZERO;
    let mut wait = FIRST_REDIAL_WAIT;
    let mut pauses = 0;
    while pause_fits(elapsed, wait) {
        elapsed += wait;
        wait = next_redial_wait(wait);
        pauses += 1;
    }

    assert_eq!(pauses, 17);
    assert_eq!(elapsed, Duration::from_secs(119));
    assert!(elapsed < REDIAL_WINDOW);
}

#[test]
fn a_pass_that_moves_nothing_leaves_what_the_viewer_paints_alone() {
    // The repaint fires on a difference, so two reads of one unchanged viewer
    // are equal.
    let client = viewer();
    let tab = TabId::new();

    assert_eq!(
        ViewerPaint::read(&client, tab),
        ViewerPaint::read(&client, tab)
    );
}

#[test]
fn a_deadline_already_past_still_takes_a_session_that_is_already_back() {
    // The client sits out its own wait for the frame that told it, so the
    // window can be spent by the time the wait runs. The file is read once
    // before the deadline is weighed, so a session that came back inside the
    // window is still joined.
    let runtime_dir = test_runtime_dir();
    let session_id = SessionId::new();
    let advertised = advertise(runtime_dir.path(), session_id, NEW_TOKEN, 4321);

    assert_eq!(
        wait_for_new_endpoint(
            runtime_dir.path(),
            session_id,
            &ConnectionToken::new(OLD_TOKEN),
            Instant::now() - Duration::from_secs(1),
        ),
        Some(advertised)
    );
}

#[test]
fn the_wait_reads_past_an_endpoint_file_that_is_not_a_file_this_build_reads() {
    // The swap rewrites the endpoint file, and a client can read it while it
    // holds bytes no build reads. That read is passed over and the wait keeps
    // reading, rather than reporting the session gone.
    let runtime_dir = test_runtime_dir();
    let session_id = SessionId::new();
    std::fs::write(EndpointFile::path(runtime_dir.path(), session_id), b"{")
        .expect("write a half endpoint file");

    let deadline = Instant::now() + Duration::from_millis(200);
    assert_eq!(
        wait_for_new_endpoint(
            runtime_dir.path(),
            session_id,
            &ConnectionToken::new(OLD_TOKEN),
            deadline,
        ),
        None
    );
    assert!(
        Instant::now() >= deadline,
        "the wait sat out its whole window rather than giving up on the first read"
    );
}
