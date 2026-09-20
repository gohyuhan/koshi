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
//! client image record this terminal holds, and every way back that fails reporting
//! the death a broken connection already reports. It also covers a remote
//! viewer whose link broke: the pause each redial waits, that dialing again
//! moves what the viewer paints, what the drain of the stretch with no link
//! keeps and what it drops, and what a viewer that stopped dialing prints and
//! exits with. It also covers the commands a fired binding's plan flattens
//! into, and how a typed value reads as a session id or as a display name.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size as RatatuiSize};

use koshi_core::action::ActionReference;
use koshi_core::command::{
    ClearSelectionArgs, CliExitCode, GridPosition, Selection, SelectionKind, SetSelectionArgs,
    VisualCommand,
};
use koshi_core::geometry::{Direction, PaneArea, Point, Rect};
use koshi_core::ids::{ClientId, PaneId, PluginId, TabId};
use koshi_core::key::{
    Key, KeyChord, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags, ModFlags,
    TEXT_ONLY_KEY_CODEPOINT,
};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseAnswer, MouseButton, MouseTracking, ScrollDirection};
use koshi_core::resolve::ActionArgs;
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::endpoint::{compute_socket_address, EndpointFile};
use koshi_ipc::frame::{FrameClient, FrameSession, FrameTab, PaintedFrame};
use koshi_ipc::protocol::{
    ConnectionToken, IncomingResponse, IpcErrorCode, IpcErrorPayload, IpcResponse, IpcResult,
    PROTOCOL_VERSION,
};
use koshi_ipc::remote_servers::SavedServer;
use koshi_ipc::router::{
    compute_router_socket_address, resolve_router_endpoint_path, RouterRequest, RouterResponse,
    ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::{Listener, MAX_FRAME_BYTE_COUNT};
use koshi_ipc::wire::MaybeKnown;
use koshi_layout::mode::LayoutMode;
use koshi_renderer::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, GridView, ImagePlacementSnapshot, MousePane,
    PaneKind, PaneSlot, PaneSnapshot, PluginUiSnapshot, RenderSnapshot, ScrollbackMeta,
    SessionSnapshot, TabMeta, TabSnapshot,
};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord,
};
use koshi_terminal::grid::state::Grid;
use koshi_terminal::style::Style;

use super::*;
use crate::tests::TEST_VIEWPORT_SIZE;
use koshi_test_support::fixtures::build_test_runtime_directory;

/// The smallest frame a session can paint: one empty tab, no panes, the client
/// unlocked. [`classify_session_event`] reads the frame's variant and nothing inside it.
fn build_test_painted_frame() -> PaintedFrame {
    build_test_painted_frame_with_lock_mode(LockMode::Normal)
}

/// A normal painted frame carrying `viewport_size` in both its client and active-tab
/// data.
fn build_test_painted_frame_with_viewport(viewport_size: Size) -> PaintedFrame {
    let mut painted_frame = build_test_painted_frame();
    painted_frame
        .session_snapshot
        .active_tab_snapshot
        .effective_cell_size = viewport_size;
    painted_frame.client_snapshot.viewport_size = viewport_size;
    painted_frame
}

/// The same frame, reporting the client in `lock_mode`. Each call mints a new
/// session id and a new tab id.
fn build_test_painted_frame_with_lock_mode(lock_mode: LockMode) -> PaintedFrame {
    let active_tab_id = TabId::new();
    PaintedFrame {
        session_snapshot: FrameSession {
            session_id: SessionId::new(),
            session_revision: 0,
            session_name: String::from("session"),
            active_tab_snapshot: FrameTab {
                tab_id: active_tab_id,
                tab_name: String::from("tab"),
                pane_slots: Vec::new(),
                effective_cell_size: Size {
                    column_count: 80,
                    row_count: 24,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tab_snapshots: Vec::new(),
        },
        pane_snapshots: Vec::new(),
        client_snapshot: FrameClient {
            client_id: ClientId::new(),
            client_revision: 0,
            viewport_size: Size {
                column_count: 80,
                row_count: 24,
            },
            active_tab_id,
            focused_pane_id: None,
            lock_mode,
            is_mouse_selection_enabled: false,
        },
    }
}

/// One listing row for a session displayed as `name`, on this machine.
fn build_test_session_row(session_name: &str) -> SessionRow {
    SessionRow {
        session_id: SessionId::new(),
        session_name: String::from(session_name),
        server_name_or_address: None,
    }
}

#[test]
fn no_running_session_leaves_nothing_to_attach_to() {
    let error = parse_session_selection(&[], "").expect_err("an empty listing has no session");
    assert_eq!(error.to_string(), "no koshi session is running");
    assert_eq!(CliExitCode::from(&error), CliExitCode::SessionNotFound);
}

#[test]
fn one_row_settles_without_reading_a_line() {
    let session_rows = vec![build_test_session_row("solo")];
    assert_eq!(
        select_session_index(&session_rows).expect("one row needs no picking"),
        0
    );
}

#[test]
fn only_a_single_local_row_settles_without_asking() {
    // (total rows, how many are local) → whether the picker skips the prompt.
    assert!(
        is_single_local_session_selection(1, 1),
        "one local session, nothing remote"
    );
    assert!(
        !is_single_local_session_selection(1, 0),
        "one remote session still asks"
    );
    assert!(
        !is_single_local_session_selection(2, 2),
        "two local sessions ask"
    );
    assert!(
        !is_single_local_session_selection(2, 1),
        "one local beside one remote asks"
    );
    assert!(
        !is_single_local_session_selection(3, 0),
        "remote-only listings ask"
    );
}

#[test]
fn a_single_row_still_needs_its_number_typed_when_picking() {
    // `choose` sends a lone remote row through `parse_session_selection`: an empty line
    // refuses instead of settling on the row.
    let session_rows = vec![build_test_session_row("solo")];
    parse_session_selection(&session_rows, "").expect_err("an empty line names no row");
    assert_eq!(
        parse_session_selection(&session_rows, "1").expect("1 is in range"),
        0
    );
}

#[test]
fn the_typed_number_picks_that_row() {
    let session_rows = vec![
        build_test_session_row("a"),
        build_test_session_row("b"),
        build_test_session_row("c"),
    ];
    assert_eq!(
        parse_session_selection(&session_rows, "2").expect("2 is in range"),
        1
    );
    assert_eq!(
        parse_session_selection(&session_rows, "2\n").expect("the read line keeps its newline"),
        1
    );
}

#[test]
fn two_rows_carrying_one_session_id_are_told_apart_by_their_place() {
    // Two rows, one session id: the answer is the place, not the id.
    let shared_session_id = SessionId::new();
    let session_rows = vec![
        SessionRow {
            session_id: shared_session_id,
            session_name: String::from("web"),
            server_name_or_address: Some(String::from("desk")),
        },
        SessionRow {
            session_id: shared_session_id,
            session_name: String::from("web"),
            server_name_or_address: Some(String::from("desk-ip")),
        },
    ];

    assert_eq!(
        parse_session_selection(&session_rows, "1").expect("1 is in range"),
        0
    );
    assert_eq!(
        parse_session_selection(&session_rows, "2").expect("2 is in range"),
        1,
        "the second row is reachable even though it shares the first row's id"
    );
}

#[test]
fn a_line_that_is_not_a_listed_number_is_refused() {
    let session_rows = vec![
        build_test_session_row("a"),
        build_test_session_row("b"),
        build_test_session_row("c"),
    ];
    for typed_input in ["0", "4", "x"] {
        let error = parse_session_selection(&session_rows, typed_input)
            .expect_err("the line names no listed row");
        assert_eq!(
            error.to_string(),
            format!(
                "invalid arguments: `{typed_input}` is not one of the listed sessions; \
                 expected a number 1 to 3"
            )
        );
        assert_eq!(CliExitCode::from(&error), CliExitCode::UsageOrConfig);
    }
}

/// A stand-in router that records the name of every request it is asked. It
/// answers a Hello with the settled protocol version and refuses every other
/// request with `BadToken`.
///
/// It serves one connection and records each request before it answers it, so
/// a caller that has its answer is a caller whose request is already in the
/// returned list. That ordering is what lets a test read the list once its
/// caller has returned, without joining the thread.
fn build_recording_router(runtime_directory: &Path) -> Arc<Mutex<Vec<&'static str>>> {
    let socket_address = compute_router_socket_address(runtime_directory);
    let listener = Listener::bind(&socket_address).expect("bind the stand-in router");
    EndpointFile {
        socket_address,
        connection_token: ConnectionToken::generate(),
        process_id: std::process::id(),
    }
    .write_to_path(&resolve_router_endpoint_path(runtime_directory))
    .expect("write the router endpoint file");

    let request_kind_names = Arc::new(Mutex::new(Vec::new()));
    let recorded_request_kind_names = Arc::clone(&request_kind_names);
    thread::spawn(move || {
        let Ok(mut connection) = listener.accept() else {
            return;
        };
        while let Ok(request) = connection.recv::<RouterRequest>() {
            recorded_request_kind_names
                .lock()
                .expect("the list outlives every panic")
                .push(request.request_kind.get_request_kind_name());
            let router_response = match request.request_kind {
                RouterRequestKind::Hello { .. } => RouterResult::Hello {
                    protocol_version: ROUTER_PROTOCOL_VERSION,
                    build_version: String::new(),
                },
                _ => RouterResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::BadToken,
                    message: String::from(
                        "the stand-in router refuses every request after the Hello",
                    ),
                }),
            };
            let _ = connection.send(&RouterResponse {
                request_id: Some(request.request_id),
                answer_result: router_response,
            });
        }
    });
    request_kind_names
}

#[test]
fn attaching_by_id_skips_the_selector_lookup() {
    let runtime_directory = build_test_runtime_directory();
    let router = build_recording_router(runtime_directory.path());
    let session_id = SessionId::new();

    let error = attach_session(runtime_directory.path(), session_id)
        .expect_err("no endpoint file advertises that session");

    let request_kind_names = router
        .lock()
        .expect("the list outlives every panic")
        .clone();
    assert_eq!(
        request_kind_names
            .iter()
            .filter(|request_kind_name| **request_kind_name == "AttachLookup")
            .count(),
        0,
        "the router was asked {request_kind_names:?}"
    );
    let CliError::SessionNotFound { session_name } = error else {
        panic!("expected SessionNotFound, got {error:?}");
    };
    assert_eq!(session_name, session_id.to_string());
}

/// A lookup by display name dials the router: the stand-in answers the Hello,
/// records both requests, and its refusal of the lookup comes back as the
/// error.
#[test]
fn looking_up_a_name_asks_the_selector_lookup() {
    let runtime_directory = build_test_runtime_directory();
    let router = build_recording_router(runtime_directory.path());

    let error = lookup_session_address(runtime_directory.path(), "quiet-lake")
        .expect_err("the stand-in router refuses the lookup");

    let request_kind_names = router
        .lock()
        .expect("the list outlives every panic")
        .clone();
    assert_eq!(request_kind_names, vec!["Hello", "AttachLookup"]);
    let CliError::IpcUnavailable { detail } = error else {
        panic!("expected IpcUnavailable, got {error:?}");
    };
    assert_eq!(
        detail,
        "the stand-in router refuses every request after the Hello"
    );
}

#[test]
fn the_detached_frame_ends_the_stream_cleanly() {
    assert_eq!(
        classify_session_event(&Ok(SessionEvent::Detached)),
        Some(AttachmentEnding::Detached)
    );
}

#[test]
fn the_quit_frame_ends_the_stream_with_the_session() {
    assert_eq!(
        classify_session_event(&Ok(SessionEvent::Quit)),
        Some(AttachmentEnding::SessionEnded)
    );
}

#[test]
fn the_switch_frame_ends_the_stream_with_the_session_to_join_next() {
    let session_id = SessionId::new();
    assert_eq!(
        classify_session_event(&Ok(SessionEvent::SwitchTo { session_id })),
        Some(AttachmentEnding::SwitchSession(session_id))
    );
}

#[test]
fn a_closed_socket_ends_the_stream_as_a_death() {
    assert_eq!(
        classify_session_event(&Err(IpcError::Disconnected)),
        Some(AttachmentEnding::ConnectionDied)
    );
}

#[test]
fn a_frame_that_does_not_decode_ends_the_stream_as_a_death() {
    let session_error = Err(IpcError::MalformedFrame {
        error_detail: "expected value".to_string(),
    });
    assert_eq!(
        classify_session_event(&session_error),
        Some(AttachmentEnding::ConnectionDied)
    );
}

#[test]
fn a_transport_failure_ends_the_stream_as_a_death() {
    let session_error = Err(IpcError::Transport {
        error_detail: "connection reset".to_string(),
    });
    assert_eq!(
        classify_session_event(&session_error),
        Some(AttachmentEnding::ConnectionDied)
    );
}

#[test]
fn every_other_frame_keeps_the_stream_reading() {
    let tab_id = TabId::new();
    let session_events = [
        SessionEvent::PaneCreated {
            pane_id: PaneId::new(),
            tab_id,
        },
        SessionEvent::PaneProcessExited {
            pane_id: PaneId::new(),
            exit_code: Some(0),
            signal: None,
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
            previous_pane_id: None,
        },
        SessionEvent::LayoutChanged { tab_id },
        SessionEvent::TabCreated { tab_id },
        SessionEvent::TabClosed { tab_id },
        SessionEvent::TabFocused {
            client_id: ClientId::new(),
            tab_id,
            previous_tab_id: TabId::new(),
        },
        SessionEvent::TabMoved {
            tab_id,
            previous_tab_index: 0,
            new_tab_index: 1,
        },
        SessionEvent::Resync {
            dropped_event_count: 3,
        },
    ];
    for session_event in session_events {
        assert_eq!(
            classify_session_event(&Ok(session_event.clone())),
            None,
            "{session_event:?}"
        );
    }
}

#[test]
fn a_painted_frame_keeps_the_stream_reading() {
    let session_event = SessionEvent::Painted {
        frame: Box::new(build_test_painted_frame()),
    };
    assert_eq!(classify_session_event(&Ok(session_event)), None);
}

#[test]
fn a_mouse_answer_keeps_the_stream_reading() {
    let session_event = SessionEvent::MouseAnswer {
        request_id: 7,
        mouse_answers: vec![MouseAnswer::Resized {
            pane_id: PaneId::new(),
            border_side: Direction::Up,
            resize_step: -1,
            applied_cell_count: 3,
        }],
    };
    assert_eq!(classify_session_event(&Ok(session_event)), None);
}

/// A session on this machine, which is the home every report test below uses
/// unless it names the remote one.
fn build_local_home() -> Home {
    Home::Local {
        runtime_directory: PathBuf::from("/tmp/koshi-test"),
    }
}

/// A session on another machine, reached through a saved server named `work`.
fn build_remote_home() -> Home {
    Home::Remote {
        server: ServerReference::Saved(SavedServer {
            server_name: Some("work".to_string()),
            server_address: "laptop.local:7654".to_string(),
            connection_token: ConnectionToken::generate(),
            certificate_fingerprint: Some("00".repeat(32)),
            added_at: SystemTime::UNIX_EPOCH,
            last_used_at: None,
        }),
    }
}

#[test]
fn a_death_reports_the_cause_and_how_to_reattach() {
    let session_id = SessionId::new();
    let error = report_attachment_ending(
        &build_local_home(),
        AttachmentEnding::ConnectionDied,
        session_id,
    )
    .expect_err("a death is an error");
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
    let error = report_attachment_ending(
        &build_remote_home(),
        AttachmentEnding::ConnectionDied,
        session_id,
    )
    .expect_err("a death is an error");
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
    let remote_home = Home::Remote {
        server: ServerReference::New {
            server_address: "laptop.local:7654".to_string(),
        },
    };
    let error =
        report_attachment_ending(&remote_home, AttachmentEnding::ConnectionDied, session_id)
            .expect_err("a death is an error");
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
    let error = report_attachment_ending(
        &build_remote_home(),
        AttachmentEnding::LinkLost(Box::new(cause)),
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
        AttachmentEnding::LinkLost(Box::new(CliError::Runtime {
            detail: detail.to_string(),
        }))
    };

    assert_eq!(lost("connection refused"), lost("connection refused"));
    assert_ne!(lost("connection refused"), lost("connection timed out"));
    assert_ne!(lost("connection refused"), AttachmentEnding::ConnectionDied);
    assert_ne!(AttachmentEnding::ConnectionDied, lost("connection refused"));
}

#[test]
fn a_refused_dial_ends_the_redial_at_once_and_is_the_cause_it_stops_on() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let mut dials = 0_u32;
    let Err(cause) = redial_remote_session_with(
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
        report_attachment_ending(&build_local_home(), AttachmentEnding::Detached, session_id)
            .expect("a detach is a success"),
        None
    );
    assert_eq!(
        report_attachment_ending(
            &build_local_home(),
            AttachmentEnding::SessionEnded,
            session_id,
        )
        .expect("a session ending is a success"),
        None
    );
}

#[test]
fn a_switch_names_the_session_to_join_next() {
    let target_session_id = SessionId::new();
    assert_eq!(
        report_attachment_ending(
            &build_local_home(),
            AttachmentEnding::SwitchSession(target_session_id),
            SessionId::new(),
        )
        .expect("a switch is a success"),
        Some(target_session_id)
    );
}

#[test]
fn the_restarting_frame_ends_the_stream_to_come_back_on_the_new_socket() {
    assert_eq!(
        classify_session_event(&Ok(SessionEvent::Restarting)),
        Some(AttachmentEnding::Restarting)
    );
}

/// Advertise `session_id` at `connection_token` and `process_id`, the way a session server does
/// every time it binds, and hand back what was written.
fn write_advertised_endpoint(
    runtime_directory: &Path,
    session_id: SessionId,
    connection_token: &str,
    process_id: u32,
) -> EndpointFile {
    let advertised_endpoint = EndpointFile {
        socket_address: compute_socket_address(runtime_directory, session_id),
        connection_token: ConnectionToken::from_secret(connection_token),
        process_id,
    };
    advertised_endpoint
        .write_to_path(&EndpointFile::resolve_endpoint_file_path(
            runtime_directory,
            session_id,
        ))
        .expect("write the session endpoint file");
    advertised_endpoint
}

/// The token a test's client attached under, which the wait watches for a
/// change.
const OLD_CONNECTION_TOKEN: &str = "the token this client attached under";

/// The token the image replacing the session mints when it binds again.
const NEW_CONNECTION_TOKEN: &str = "the token the new image minted";

#[test]
fn the_wait_takes_the_endpoint_file_the_moment_it_names_another_token() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let advertised = write_advertised_endpoint(
        runtime_directory.path(),
        session_id,
        NEW_CONNECTION_TOKEN,
        4321,
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    assert_eq!(
        wait_for_new_endpoint(
            runtime_directory.path(),
            session_id,
            &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
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
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    write_advertised_endpoint(
        runtime_directory.path(),
        session_id,
        OLD_CONNECTION_TOKEN,
        4321,
    );

    let runtime_directory_path = runtime_directory.path().to_path_buf();
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        write_advertised_endpoint(
            &runtime_directory_path,
            session_id,
            NEW_CONNECTION_TOKEN,
            4321,
        )
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let new_endpoint = wait_for_new_endpoint(
        runtime_directory.path(),
        session_id,
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        deadline,
    );
    let advertised = writer.join().expect("the writing thread finished");
    assert_eq!(new_endpoint, Some(advertised));
    assert!(
        Instant::now() < deadline,
        "the wait returned before its deadline"
    );
}

#[test]
fn the_wait_ignores_an_endpoint_file_still_naming_the_token_this_client_attached_under() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    // A different process id under the same token. On Unix `execvp` keeps the
    // process id, so only the token tells the new image from the old one.
    write_advertised_endpoint(
        runtime_directory.path(),
        session_id,
        OLD_CONNECTION_TOKEN,
        9999,
    );

    assert_eq!(
        wait_for_new_endpoint(
            runtime_directory.path(),
            session_id,
            &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
            Instant::now(),
        ),
        None
    );
}

#[test]
fn the_wait_ends_at_its_deadline_when_the_session_advertises_nothing() {
    let runtime_directory = build_test_runtime_directory();
    // No endpoint file at all, which is what the swap leaves while the
    // session's socket is down.
    assert_eq!(
        wait_for_new_endpoint(
            runtime_directory.path(),
            SessionId::new(),
            &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
            Instant::now(),
        ),
        None
    );
}

/// Build an Attach answer naming `client_id` and `session_id`, echoing
/// `pane_area`.
fn build_attached_response(
    client_id: ClientId,
    session_id: SessionId,
    pane_area: Option<PaneArea>,
) -> IncomingResponse {
    IpcResponse {
        request_id: Some(2),
        answer_result: MaybeKnown::Known(IpcResult::Attached {
            client_id,
            session_id,
            session_structure: AttachedSessionStructureSnapshot {
                session_id,
                session_name: String::from("session"),
                tabs: Vec::new(),
                panes: Vec::new(),
            },
            resume_token: None,
            pane_area,
        }),
    }
}

/// A stand-in session that has just come back from replacing its own process
/// image: it binds `session_id`'s socket, advertises it under [`NEW_CONNECTION_TOKEN`],
/// and serves one connection.
///
/// `attached` is how it answers the Attach: `Ok` is the client id it hands back,
/// `Err` is the sentence it refuses with. It records the Attach it read before
/// it answers, so a caller holding that answer is a caller whose request is
/// already in the returned slot.
fn spawn_restarted_session(
    runtime_directory: &Path,
    session_id: SessionId,
    attached: Result<ClientId, &'static str>,
) -> Arc<Mutex<Option<IpcRequestKind>>> {
    let socket_address = compute_socket_address(runtime_directory, session_id);
    let listener = Listener::bind(&socket_address).expect("bind the stand-in session");
    write_advertised_endpoint(
        runtime_directory,
        session_id,
        NEW_CONNECTION_TOKEN,
        std::process::id(),
    );

    let recorded_request_kind = Arc::new(Mutex::new(None));
    let request_kind_slot = Arc::clone(&recorded_request_kind);
    thread::spawn(move || {
        let Ok(mut connection) = listener.accept() else {
            return;
        };
        while let Ok(request) = connection.recv::<IpcRequest>() {
            let ipc_response = match &request.request_kind {
                IpcRequestKind::Hello { .. } => IpcResult::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    build_version: String::from("0.0.0"),
                },
                IpcRequestKind::Attach { .. } => {
                    *request_kind_slot
                        .lock()
                        .expect("the slot outlives every panic") =
                        Some(request.request_kind.clone());
                    match attached {
                        Ok(client_id) => IpcResult::Attached {
                            client_id,
                            session_id,
                            session_structure: AttachedSessionStructureSnapshot {
                                session_id,
                                session_name: String::from("session"),
                                tabs: Vec::new(),
                                panes: Vec::new(),
                            },
                            resume_token: None,
                            pane_area: None,
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
                answer_result: ipc_response,
            });
        }
    });
    recorded_request_kind
}

#[test]
fn an_attach_answer_is_taken_whether_or_not_it_echoes_a_pane_area() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();

    let echoed = parse_attached_session(build_attached_response(
        client_id,
        session_id,
        Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 22,
        })),
    ))
    .expect("the answer of a session that echoes the field is accepted");
    let silent = parse_attached_session(build_attached_response(client_id, session_id, None))
        .expect("the answer of an older session is accepted too");

    assert_eq!(echoed, (client_id, session_id, None));
    assert_eq!(silent, (client_id, session_id, None));
}

#[test]
fn coming_back_after_a_restart_keeps_the_client_record_and_graphics_capability() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let recorded_attach_request =
        spawn_restarted_session(runtime_directory.path(), session_id, Ok(client_id));
    let advertised_endpoint = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), session_id),
    )
    .expect("the stand-in session advertised its socket");

    let (rejoined_endpoint, client_connection) = rejoin_session(
        runtime_directory.path(),
        session_id,
        client_id,
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        terminal::GraphicsSupport::Kitty,
        None,
    )
    .expect("the stand-in session handed the client image record back");
    drop(client_connection);

    assert_eq!(rejoined_endpoint, advertised_endpoint);
    assert_eq!(
        recorded_attach_request
            .lock()
            .expect("the slot outlives every panic")
            .clone(),
        Some(IpcRequestKind::Attach {
            viewport: get_terminal_viewport_size(),
            event_filter: EventFilterSpec::All,
            resume_client_id: Some(client_id),
            resume_token: None,
            pane_area: Some(compute_core_pane_area(get_terminal_viewport_size())),
            graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities {
                supports_kitty: true,
                supports_iterm: false,
                supports_sixel: false,
            },
            cell_size: None,
        }),
        "the restart path claims the client image record and reports this terminal's capability"
    );
}

#[test]
fn attach_advertises_the_selected_native_protocol() {
    let iterm = build_attach_request(None, None, terminal::GraphicsSupport::Iterm, None);
    let sixel = build_attach_request(
        None,
        None,
        terminal::GraphicsSupport::Sixel {
            palette_color_count: 2,
            max_pixel_width: None,
            max_pixel_height: None,
        },
        None,
    );

    let IpcRequestKind::Attach {
        graphics_capabilities,
        ..
    } = iterm.request_kind
    else {
        panic!("expected an attach request");
    };
    assert_eq!(
        graphics_capabilities,
        koshi_ipc::protocol::GraphicsCapabilities {
            supports_kitty: false,
            supports_iterm: true,
            supports_sixel: false,
        }
    );
    let IpcRequestKind::Attach {
        graphics_capabilities,
        ..
    } = sixel.request_kind
    else {
        panic!("expected an attach request");
    };
    assert_eq!(
        graphics_capabilities,
        koshi_ipc::protocol::GraphicsCapabilities {
            supports_kitty: false,
            supports_iterm: false,
            supports_sixel: true,
        }
    );
}

#[test]
fn a_restarted_session_that_refuses_the_join_leaves_nothing_to_come_back_to() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let _recorded_attach_request = spawn_restarted_session(
        runtime_directory.path(),
        session_id,
        Err("this session holds no such client"),
    );

    let attached = rejoin_session(
        runtime_directory.path(),
        session_id,
        client_id,
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        terminal::GraphicsSupport::Unsupported,
        None,
    );
    assert_eq!(
        attached.map(|(rejoined_endpoint, _)| rejoined_endpoint),
        None
    );
}

#[test]
fn a_restarted_session_that_mints_a_new_client_leaves_nothing_to_come_back_to() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let _recorded_attach_request =
        spawn_restarted_session(runtime_directory.path(), session_id, Ok(ClientId::new()));

    let attached = rejoin_session(
        runtime_directory.path(),
        session_id,
        ClientId::new(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        terminal::GraphicsSupport::Unsupported,
        None,
    );
    assert_eq!(
        attached.map(|(rejoined_endpoint, _)| rejoined_endpoint),
        None
    );
}

#[test]
fn another_local_users_restarting_session_is_not_waited_for() {
    let runtime_directory = build_test_runtime_directory();
    let redial_started_at = Instant::now();

    // The empty token is what a session another local user started is reached
    // with: this user reads no endpoint file for it, so there is none to watch.
    let attached = rejoin_session(
        runtime_directory.path(),
        SessionId::new(),
        ClientId::new(),
        &ConnectionToken::from_secret(""),
        terminal::GraphicsSupport::Unsupported,
        None,
    );

    assert_eq!(
        attached.map(|(rejoined_endpoint, _)| rejoined_endpoint),
        None
    );
    assert!(
        redial_started_at.elapsed() < Duration::from_secs(1),
        "no wait was spent on a session this client cannot watch"
    );
}

#[test]
fn a_restart_this_client_cannot_come_back_from_reports_the_death_it_reports_today() {
    let session_id = SessionId::new();
    let error = report_attachment_ending(
        &build_local_home(),
        AttachmentEnding::Restarting,
        session_id,
    )
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

/// A test client on the stock settings — `scroll_line_count` 3, wheel scrolls scrollback,
/// border resize on. Its subscription has no sender: frames reach an attached
/// client over the connection instead.
fn build_test_client() -> Client {
    crate::tests::build_test_client_with_event_sender().0
}

/// One plain terminal pane: no highlight, no mouse mode, on the primary screen.
fn build_plain_mouse_pane(pane_id: PaneId) -> MousePane {
    MousePane {
        pane_id,
        view_top_row_index: 0,
        mouse_tracking: MouseTracking::Off,
        is_alternate_scroll_enabled: false,
        is_on_alternate_screen: false,
        has_selection: false,
    }
}

/// A frame of `mouse_panes`, laid out as full-width horizontal bands between the
/// tabline (row 0) and the hint bar (last row), with the first pane focused.
///
/// Two panes in an 80x24 viewport gives band rows 1..=11 and 12..=22, each with
/// a one-cell border ring, so the second band's top row is the divider the two
/// panes share.
fn build_mouse_frame(mouse_panes: &[MousePane]) -> MouseFrame {
    let tab_id = TabId::new();
    let band =
        (TEST_VIEWPORT_SIZE.row_count - 2) / u16::try_from(mouse_panes.len()).expect("few panes");
    let pane_slots: Vec<PaneSlot> = mouse_panes
        .iter()
        .enumerate()
        .map(|(pane_index, mouse_pane)| {
            let top_row = band * u16::try_from(pane_index).expect("few panes");
            PaneSlot {
                pane_id: mouse_pane.pane_id,
                outer_rect: Rect::from_origin_and_size(
                    Point {
                        column: 0,
                        row: top_row,
                    },
                    Size {
                        column_count: TEST_VIEWPORT_SIZE.column_count,
                        row_count: band,
                    },
                ),
                content_rect: Some(Rect::from_origin_and_size(
                    Point {
                        column: 1,
                        row: top_row + 1,
                    },
                    Size {
                        column_count: TEST_VIEWPORT_SIZE.column_count - 2,
                        row_count: band - 2,
                    },
                )),
                pane_kind: PaneKind::Terminal,
                is_visible: true,
                is_suppressed: false,
                is_dead: false,
            }
        })
        .collect();
    MouseFrame {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_revision: 0,
            session_name: String::from("fixture"),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: String::from("one"),
                pane_slots,
                effective_cell_size: Size {
                    column_count: TEST_VIEWPORT_SIZE.column_count,
                    row_count: TEST_VIEWPORT_SIZE.row_count - 2,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMeta {
                tab_id,
                tab_name: String::from("one"),
                tab_index: 0,
                is_active: true,
            }],
        },
        mouse_panes: mouse_panes.to_vec(),
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            client_revision: 0,
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id: mouse_panes.first().map(|mouse_pane| mouse_pane.pane_id),
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
        committed_regions: CommittedRegions::core(TEST_VIEWPORT_SIZE, 0),
    }
}

/// A cell inside the content of the `pane_index`-th pane in a fixture frame.
fn get_content_cell(mouse_layout_frame: &MouseFrame, pane_index: usize) -> Point {
    let content_rect = mouse_layout_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[pane_index]
        .content_rect
        .expect("a visible pane");
    Point {
        column: content_rect.origin.column + 1,
        row: content_rect.origin.row + 2,
    }
}

/// The row the two bands of a two-pane fixture frame share: the second pane's
/// top border.
fn get_border_divider_point(mouse_layout_frame: &MouseFrame) -> Point {
    Point {
        column: 10,
        row: mouse_layout_frame
            .session_snapshot
            .active_tab_snapshot
            .pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    }
}

/// A wheel tick at `screen_point`.
fn build_mouse_wheel(scroll_direction: ScrollDirection, screen_point: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Scroll(scroll_direction),
        position: screen_point,
        modifier_flags: ModFlags::NONE,
    }
}

/// A left press at `screen_point`.
fn build_mouse_press(screen_point: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Press(MouseButton::Left),
        position: screen_point,
        modifier_flags: ModFlags::NONE,
    }
}

/// A left drag to `screen_point`.
fn build_mouse_drag(screen_point: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Drag(MouseButton::Left),
        position: screen_point,
        modifier_flags: ModFlags::NONE,
    }
}

/// A left release at `screen_point`.
fn build_mouse_release(screen_point: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Release(MouseButton::Left),
        position: screen_point,
        modifier_flags: ModFlags::NONE,
    }
}

/// One highlight change for `pane_id`, ending at line `row_index`.
fn build_selection_action(pane_id: PaneId, row_index: u64) -> MouseAction {
    MouseAction::Command(Command::Visual(VisualCommand::SetSelection(
        SetSelectionArgs {
            pane_id,
            selection: Selection {
                selection_kind: SelectionKind::Character,
                anchor: GridPosition {
                    row_index: 0,
                    column_index: 0,
                },
                cursor: GridPosition {
                    row_index,
                    column_index: 4,
                },
            },
        },
    )))
}

/// One press handed to `pane_id`'s program, at `column_index`.
fn build_forward_mouse_action(pane_id: PaneId, column_index: u16) -> MouseAction {
    MouseAction::Forward {
        pane_id,
        mouse_input: build_mouse_press(Point {
            column: column_index,
            row: 5,
        }),
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
    /// The client end, numbered from [`FIRST_POST_ATTACH_REQUEST_ID`] exactly as the
    /// loop numbers it.
    uplink: Uplink,
}

fn build_test_wire() -> Wire {
    // The address is a socket-file path on Unix and a bare pipe name on
    // Windows, so the directory goes unused there. `/tmp` keeps the path short
    // enough for the platform's socket-path limit; the session id makes it
    // unique, so tests running side by side never share one.
    #[cfg(unix)]
    let socket_directory = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let socket_directory = std::env::temp_dir();
    let socket_address = compute_socket_address(&socket_directory, SessionId::new());
    let listener = Listener::bind(&socket_address).expect("bind the control socket");
    let accept_thread = thread::spawn(move || listener.accept().expect("accept the connection"));
    let client_connection =
        Connection::connect(&socket_address).expect("connect to the control socket");
    let session_connection = accept_thread.join().expect("the accepting thread finished");
    let (frame_reader, frame_writer) = client_connection.split();
    Wire {
        _reader: frame_reader,
        session: session_connection,
        uplink: Uplink {
            request_sender: spawn_uplink_writer(frame_writer),
            registry: ActionRegistry::new(),
            next_request_id: FIRST_POST_ATTACH_REQUEST_ID,
        },
    }
}

#[test]
fn three_ticks_over_one_pane_fold_to_one_scroll_of_the_summed_lines() {
    let pane_id = PaneId::new();
    let scroll_actions = vec![
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        },
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        },
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        },
    ];

    assert_eq!(
        coalesce_mouse_actions(scroll_actions),
        vec![MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 9,
        }]
    );
}

#[test]
fn ticks_over_two_panes_stay_two_scrolls() {
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let scroll_actions = vec![
        MouseAction::Scroll {
            pane_id: first_pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        },
        MouseAction::Scroll {
            pane_id: second_pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        },
    ];

    assert_eq!(
        coalesce_mouse_actions(scroll_actions.clone()),
        scroll_actions
    );
}

#[test]
fn ticks_in_opposite_directions_stay_two_scrolls() {
    let pane_id = PaneId::new();
    let scroll_actions = vec![
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        },
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up: false,
            scroll_line_count: 3,
        },
    ];

    assert_eq!(
        coalesce_mouse_actions(scroll_actions.clone()),
        scroll_actions
    );
}

#[test]
fn two_alternate_scroll_runs_over_one_pane_sum_their_arrows() {
    let pane_id = PaneId::new();
    let alternate_scroll_actions = vec![
        MouseAction::AltScrollArrows {
            pane_id,
            is_scrolling_up: false,
            arrow_count: 3,
        },
        MouseAction::AltScrollArrows {
            pane_id,
            is_scrolling_up: false,
            arrow_count: 5,
        },
    ];

    assert_eq!(
        coalesce_mouse_actions(alternate_scroll_actions),
        vec![MouseAction::AltScrollArrows {
            pane_id,
            is_scrolling_up: false,
            arrow_count: 8,
        }]
    );
}

#[test]
fn two_highlight_changes_for_one_pane_keep_the_newer() {
    let pane_id = PaneId::new();

    assert_eq!(
        coalesce_mouse_actions(vec![
            build_selection_action(pane_id, 12),
            build_selection_action(pane_id, 40)
        ]),
        vec![build_selection_action(pane_id, 40)]
    );
}

#[test]
fn two_border_moves_for_one_pane_and_side_keep_the_newer_and_do_not_sum() {
    let pane_id = PaneId::new();
    let resize_actions = vec![
        MouseAction::Resize {
            pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 2,
        },
        MouseAction::Resize {
            pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 5,
        },
    ];

    assert_eq!(
        coalesce_mouse_actions(resize_actions),
        vec![MouseAction::Resize {
            pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 5,
        }],
        "each step is measured from the same anchor, so the newest is the whole move"
    );
}

#[test]
fn border_moves_on_two_sides_of_one_pane_stay_two_moves() {
    let pane_id = PaneId::new();
    let resize_actions = vec![
        MouseAction::Resize {
            pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 2,
        },
        MouseAction::Resize {
            pane_id,
            border_side: Direction::Left,
            resize_step: 1,
            requested_cell_count: 4,
        },
    ];

    assert_eq!(
        coalesce_mouse_actions(resize_actions.clone()),
        resize_actions
    );
}

#[test]
fn two_forwards_are_never_folded() {
    let pane_id = PaneId::new();
    let forwarded_actions = vec![
        build_forward_mouse_action(pane_id, 4),
        build_forward_mouse_action(pane_id, 4),
    ];

    assert_eq!(
        coalesce_mouse_actions(forwarded_actions.clone()),
        forwarded_actions
    );
}

#[test]
fn a_forward_between_two_scrolls_keeps_all_three() {
    let pane_id = PaneId::new();
    let pending_mouse_actions = vec![
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        },
        build_forward_mouse_action(pane_id, 4),
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        },
    ];

    assert_eq!(
        coalesce_mouse_actions(pending_mouse_actions.clone()),
        pending_mouse_actions,
        "only neighbours fold, so the forward keeps the two scrolls apart"
    );
}

#[test]
fn an_empty_round_takes_no_request_id_and_writes_nothing() {
    let mut wire = build_test_wire();

    assert_eq!(send_mouse_round(&mut wire.uplink, Vec::new()), None);

    // The next request written is the first thing on the wire, and it carries
    // the id the empty round would have taken.
    let sentinel = wire.uplink.send_request(IpcRequestKind::Discovery);
    assert_eq!(sentinel, FIRST_POST_ATTACH_REQUEST_ID);
    let request: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn a_round_goes_out_as_one_request_holding_its_actions_in_order() {
    let pane_id = PaneId::new();
    let mut wire = build_test_wire();
    let round = vec![
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 9,
        },
        build_forward_mouse_action(pane_id, 4),
        MouseAction::Resize {
            pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 5,
        },
    ];

    assert_eq!(
        send_mouse_round(&mut wire.uplink, round),
        Some(FIRST_POST_ATTACH_REQUEST_ID)
    );

    let request: IpcRequest = wire.session.recv().expect("read the round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(vec![
                WireMouseAction::Scroll {
                    pane_id,
                    is_scrolling_up: true,
                    scroll_line_count: 9,
                },
                WireMouseAction::Forward {
                    pane_id,
                    mouse_input: build_mouse_press(Point { column: 4, row: 5 }),
                },
                WireMouseAction::Resize {
                    pane_id,
                    border_side: Direction::Up,
                    resize_step: -1,
                    requested_cell_count: 5,
                },
            ]),
        }
    );

    // Nothing follows it: the whole round was one request.
    let sentinel = wire.uplink.send_request(IpcRequestKind::Discovery);
    let sentinel_request: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        sentinel_request,
        IpcRequest {
            request_id: sentinel,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

/// How many full rounds the writer-behind test queues while the session end
/// reads nothing.
///
/// Every round holds [`MAX_PENDING_MOUSE_ACTION_COUNT`] forwarded reports, which is about
/// 31 kB of frame, so 128 rounds come to about 3.9 MB — past every platform's
/// socket buffer, which is at most a few hundred kilobytes — and the writer
/// thread is therefore stuck inside a write for all but the first few.
const WRITER_STALLED_ROUND_COUNT: u64 = 128;

/// How long the test waits for the queueing to finish before it calls the loop
/// stalled. A watchdog, not a deadline the code observes: the queueing takes
/// milliseconds, and a write done on the queueing thread would never finish at
/// all.
const QUEUEING_WATCHDOG_TIMEOUT_DURATION: Duration = Duration::from_secs(60);

/// One round of forwarded reports, each at its own column so the round read
/// back off the wire can be checked report for report.
fn build_full_forwarded_mouse_round(pane_id: PaneId) -> Vec<MouseAction> {
    (0..MAX_PENDING_MOUSE_ACTION_COUNT)
        .map(|column_index| {
            build_forward_mouse_action(
                pane_id,
                u16::try_from(column_index).expect("the cap fits a u16"),
            )
        })
        .collect()
}

/// The same round in its wire spelling.
fn build_full_forwarded_wire_round(pane_id: PaneId) -> Vec<WireMouseAction> {
    (0..MAX_PENDING_MOUSE_ACTION_COUNT)
        .map(|column_index| WireMouseAction::Forward {
            pane_id,
            mouse_input: build_mouse_press(Point {
                column: u16::try_from(column_index).expect("the cap fits a u16"),
                row: 5,
            }),
        })
        .collect()
}

#[test]
fn queueing_a_round_returns_while_a_full_socket_holds_the_writer() {
    let pane_id = PaneId::new();
    let Wire {
        _reader,
        mut session,
        mut uplink,
    } = build_test_wire();
    let (done_tx, done_rx) = mpsc::channel();

    // The session end reads nothing until every round is queued, so the socket
    // fills and the writer thread stops inside a write. The rounds are queued
    // on their own thread, which stands in for the loop: a write done there
    // would stop with the socket and this send would never return.
    let queueing = thread::spawn(move || {
        for round in 0..WRITER_STALLED_ROUND_COUNT {
            assert_eq!(
                send_mouse_round(&mut uplink, build_full_forwarded_mouse_round(pane_id)),
                Some(FIRST_POST_ATTACH_REQUEST_ID + round),
            );
        }
        done_tx.send(()).expect("the test is still waiting");
    });
    done_rx
        .recv_timeout(QUEUEING_WATCHDOG_TIMEOUT_DURATION)
        .expect("every round was queued while the writer was stuck on the full socket");

    // Reading drains the socket and lets the writer run out. Every round
    // arrives whole and in the order it was queued: the backed-up queue drops
    // no forwarded report, folds none into another, and reorders none.
    for round in 0..WRITER_STALLED_ROUND_COUNT {
        let request: IpcRequest = session.recv().expect("read the round");
        assert_eq!(
            request,
            IpcRequest {
                request_id: FIRST_POST_ATTACH_REQUEST_ID + round,
                request_kind: IpcRequestKind::Mouse(build_full_forwarded_wire_round(pane_id)),
            }
        );
    }
    queueing.join().expect("the queueing thread finished");
}

/// A viewer holding a border drag three cells past the divider it grabbed, and
/// the frame it decided against. The drag is what a matching `Resized` moves
/// and a stale one leaves alone.
fn build_border_drag_fixture() -> (Client, MouseFrame, Point) {
    let mouse_frame = build_mouse_frame(&[
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(PaneId::new()),
    ]);
    let grabbed_divider_point = get_border_divider_point(&mouse_frame);
    let pointer_point = Point {
        row: grabbed_divider_point.row + 3,
        ..grabbed_divider_point
    };
    let mut client = build_test_client();
    let current_time = Instant::now();

    client.handle_mouse(
        build_mouse_press(grabbed_divider_point),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        client.handle_mouse(build_mouse_drag(pointer_point), &mouse_frame, current_time),
        vec![MouseAction::Resize {
            pane_id: mouse_frame.mouse_panes[1].pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        }],
        "three cells of travel away from the grabbed top border"
    );
    (client, mouse_frame, pointer_point)
}

/// The session's answer to a move of the top border [`dragging_a_border`]
/// grabbed on `pane_id`, `applied_cell_count` cells of it accepted.
fn build_top_border_resize_answer(pane_id: PaneId, applied_cell_count: u16) -> MouseAnswer {
    MouseAnswer::Resized {
        pane_id,
        border_side: Direction::Up,
        resize_step: -1,
        applied_cell_count,
    }
}

/// The image record of a move of `pane_id`'s top border, `requested_cell_count` cells inward, written in
/// round `request_id` and not yet answered.
fn build_sent_top_border_move(
    request_id: u64,
    pane_id: PaneId,
    requested_cell_count: u16,
) -> SentBorderMove {
    SentBorderMove {
        request_id,
        pane_id,
        border_side: Direction::Up,
        requested_cell_delta: -i32::from(requested_cell_count),
    }
}

#[test]
fn an_answer_moves_the_drag_anchor_whatever_round_it_came_back_in() {
    let (mut client, frame, to) = build_border_drag_fixture();
    let mut sent = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    apply_mouse_answers(
        &mut client,
        &frame,
        &mut sent,
        8,
        vec![build_top_border_resize_answer(
            frame.mouse_panes[1].pane_id,
            3,
        )],
        &mut pending_mouse_actions,
    );

    assert_eq!(sent, Vec::new());
    assert_eq!(pending_mouse_actions, Vec::new());
    assert_eq!(
        client.handle_mouse(build_mouse_drag(to), &frame, Instant::now()),
        Vec::new(),
        "the answer names its own border, so the anchor moved to the pointer"
    );
}

#[test]
fn a_resized_forgets_only_the_border_move_its_own_round_wrote() {
    let (mut client, frame, _to) = build_border_drag_fixture();
    let pane_id = frame.mouse_panes[1].pane_id;
    let mut sent = vec![
        build_sent_top_border_move(7, pane_id, 3),
        build_sent_top_border_move(8, pane_id, 1),
    ];
    let mut pending_mouse_actions = Vec::new();

    apply_mouse_answers(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![build_top_border_resize_answer(pane_id, 3)],
        &mut pending_mouse_actions,
    );

    assert_eq!(
        sent,
        vec![build_sent_top_border_move(8, pane_id, 1)],
        "round 8's move is still on the wire"
    );
    assert_eq!(
        compute_sent_border_cell_delta(&sent, pane_id, Direction::Up),
        -1
    );
}

#[test]
fn an_empty_answer_changes_nothing() {
    let (mut client, frame, to) = build_border_drag_fixture();
    let mut sent = vec![build_sent_top_border_move(
        7,
        frame.mouse_panes[1].pane_id,
        3,
    )];
    let mut pending_mouse_actions = Vec::new();

    apply_mouse_answers(
        &mut client,
        &frame,
        &mut sent,
        7,
        Vec::new(),
        &mut pending_mouse_actions,
    );

    assert_eq!(
        sent,
        vec![build_sent_top_border_move(
            7,
            frame.mouse_panes[1].pane_id,
            3
        )],
        "a round that reported no border move forgets none"
    );
    assert_eq!(pending_mouse_actions, Vec::new());
    assert_eq!(
        client.handle_mouse(build_mouse_drag(to), &frame, Instant::now()),
        vec![MouseAction::Resize {
            pane_id: frame.mouse_panes[1].pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        }],
        "nothing was reported, so the anchor stayed where it was"
    );
}

#[test]
fn a_tick_reaches_the_wire_at_once_with_rounds_already_out() {
    let pane_id = PaneId::new();
    let frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let screen_point = get_content_cell(&frame, 0);
    let mut client = build_test_client();
    let mut wire = build_test_wire();
    let mut sent = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    // Four ticks, one per pass of the loop, none of them answered. Every pass
    // writes what it decided, so four rounds are on the wire unanswered.
    for round in 0..4u64 {
        handle_mouse_event(
            &mut client,
            &frame,
            build_mouse_wheel(ScrollDirection::Up, screen_point),
            &mut pending_mouse_actions,
        );
        flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);
        assert_eq!(
            pending_mouse_actions,
            Vec::new(),
            "the pass that decided it wrote it"
        );
        let request: IpcRequest = wire.session.recv().expect("read the round");
        assert_eq!(
            request,
            IpcRequest {
                request_id: FIRST_POST_ATTACH_REQUEST_ID + round,
                request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                    pane_id,
                    is_scrolling_up: true,
                    scroll_line_count: 3,
                }]),
            }
        );
    }

    // A fifth tick with those four still unanswered. It is written before the
    // sentinel that follows it, so it is the first request read off the wire.
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_wheel(ScrollDirection::Up, screen_point),
        &mut pending_mouse_actions,
    );
    flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);
    let sentinel = wire.uplink.send_request(IpcRequestKind::Discovery);

    let request: IpcRequest = wire.session.recv().expect("read the fifth round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID + 4,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: true,
                scroll_line_count: 3,
            }]),
        },
        "four unanswered rounds held the fifth tick back by nothing"
    );
    let sentinel_request: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        sentinel_request,
        IpcRequest {
            request_id: sentinel,
            request_kind: IpcRequestKind::Discovery,
        }
    );
    assert_eq!(sent, Vec::new(), "no border move was written");
}

#[test]
fn ten_ticks_in_one_pass_leave_as_one_scroll_of_the_summed_lines() {
    let pane_id = PaneId::new();
    let frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let screen_point = get_content_cell(&frame, 0);
    let mut client = build_test_client();
    let mut wire = build_test_wire();
    let mut sent = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    // A burst of ten ticks read out of the channel as one batch, so all ten are
    // decided before the pass writes. No clock is read anywhere here.
    for _ in 0..10 {
        handle_mouse_event(
            &mut client,
            &frame,
            build_mouse_wheel(ScrollDirection::Up, screen_point),
            &mut pending_mouse_actions,
        );
    }
    assert_eq!(
        pending_mouse_actions,
        vec![
            MouseAction::Scroll {
                pane_id,
                is_scrolling_up: true,
                scroll_line_count: 3,
            };
            10
        ],
        "one three-line scroll per tick, none written yet"
    );

    flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);

    assert_eq!(pending_mouse_actions, Vec::new());
    let request: IpcRequest = wire.session.recv().expect("read the folded burst");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: true,
                scroll_line_count: 30,
            }]),
        },
        "ten ticks of three lines each, as one scroll of thirty"
    );

    // Nothing follows it: the whole burst was one request.
    let sentinel = wire.uplink.send_request(IpcRequestKind::Discovery);
    let sentinel_request: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        sentinel_request,
        IpcRequest {
            request_id: sentinel,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn a_run_of_drag_moves_in_one_pass_leaves_as_the_newest_highlight() {
    let pane_id = PaneId::new();
    let frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let mut client = build_test_client();
    let mut wire = build_test_wire();
    let mut sent = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    // The press arrives in a pass of its own and goes out as its own round, so
    // the drag moves below are the only thing the next pass holds.
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_press(get_content_cell(&frame, 0)),
        &mut pending_mouse_actions,
    );
    flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);
    let request: IpcRequest = wire.session.recv().expect("read the press");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Command(Box::new(
                Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
                    pane_id
                }))
            ))]),
        }
    );

    // Three drag moves arriving together in the next pass. The pane's content
    // starts at screen cell (1, 2) and shows line 0 on its top row, so the
    // pointer at (20, 12) names line 10, column 19.
    for screen_point in [
        Point { column: 5, row: 5 },
        Point { column: 9, row: 8 },
        Point {
            column: 20,
            row: 12,
        },
    ] {
        handle_mouse_event(
            &mut client,
            &frame,
            build_mouse_drag(screen_point),
            &mut pending_mouse_actions,
        );
    }
    assert_eq!(
        pending_mouse_actions,
        [
            GridPosition {
                row_index: 3,
                column_index: 4
            },
            GridPosition {
                row_index: 6,
                column_index: 8
            },
            GridPosition {
                row_index: 10,
                column_index: 19
            },
        ]
        .map(
            |cursor| MouseAction::Command(Command::Visual(VisualCommand::SetSelection(
                SetSelectionArgs {
                    pane_id,
                    selection: Selection {
                        selection_kind: SelectionKind::Character,
                        anchor: GridPosition {
                            row_index: 1,
                            column_index: 1
                        },
                        cursor,
                    },
                }
            )))
        )
        .to_vec(),
        "one whole highlight per move, none written yet"
    );

    flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);

    assert_eq!(pending_mouse_actions, Vec::new());
    let request: IpcRequest = wire.session.recv().expect("read the folded run");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID + 1,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Command(Box::new(
                Command::Visual(VisualCommand::SetSelection(SetSelectionArgs {
                    pane_id,
                    selection: Selection {
                        selection_kind: SelectionKind::Character,
                        anchor: GridPosition { row_index: 1, column_index: 1 },
                        cursor: GridPosition { row_index: 10, column_index: 19 },
                    },
                }))
            ))]),
        },
        "the anchor is the press and the cursor is the last move, with nothing of the two before it"
    );

    // Nothing follows it: the whole run was one request.
    let sentinel = wire.uplink.send_request(IpcRequestKind::Discovery);
    let sentinel_request: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        sentinel_request,
        IpcRequest {
            request_id: sentinel,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn every_report_in_one_pass_leaves_as_its_own_forward_in_order() {
    let plain = PaneId::new();
    let watched = PaneId::new();
    let frame = build_mouse_frame(&[
        build_plain_mouse_pane(plain),
        MousePane {
            mouse_tracking: MouseTracking::Normal,
            ..build_plain_mouse_pane(watched)
        },
    ]);
    let over_watched = get_content_cell(&frame, 1);
    let mut client = build_test_client();
    let mut wire = build_test_wire();
    let mut sent = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    // Five ticks on the pane whose program reads the mouse, arriving together in
    // one pass. The first two are the same tick twice: the pair a fold would
    // join.
    let forwarded_mouse_inputs = [
        build_mouse_wheel(ScrollDirection::Up, over_watched),
        build_mouse_wheel(ScrollDirection::Up, over_watched),
        build_mouse_wheel(ScrollDirection::Down, over_watched),
        build_mouse_wheel(
            ScrollDirection::Up,
            Point {
                column: over_watched.column + 1,
                ..over_watched
            },
        ),
        build_mouse_wheel(
            ScrollDirection::Up,
            Point {
                row: over_watched.row + 1,
                ..over_watched
            },
        ),
    ];
    for mouse_input in forwarded_mouse_inputs {
        handle_mouse_event(&mut client, &frame, mouse_input, &mut pending_mouse_actions);
    }

    let forwarded_actions: Vec<MouseAction> = forwarded_mouse_inputs
        .iter()
        .map(|&mouse_input| MouseAction::Forward {
            pane_id: watched,
            mouse_input,
        })
        .collect();
    assert_eq!(
        pending_mouse_actions, forwarded_actions,
        "one report per tick, none written yet"
    );

    flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);

    assert_eq!(pending_mouse_actions, Vec::new());
    let request: IpcRequest = wire.session.recv().expect("read the round of reports");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(
                forwarded_mouse_inputs
                    .iter()
                    .map(|&mouse_input| WireMouseAction::Forward {
                        pane_id: watched,
                        mouse_input,
                    })
                    .collect(),
            ),
        },
        "five reports in, five reports out, in the order they happened"
    );

    // Nothing follows it: the five reports were one request.
    let sentinel = wire.uplink.send_request(IpcRequestKind::Discovery);
    let sentinel_request: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        sentinel_request,
        IpcRequest {
            request_id: sentinel,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn the_pile_stops_at_the_cap_and_keeps_every_forwarded_report() {
    let plain = PaneId::new();
    let watched = PaneId::new();
    let frame = build_mouse_frame(&[
        build_plain_mouse_pane(plain),
        MousePane {
            mouse_tracking: MouseTracking::Normal,
            ..build_plain_mouse_pane(watched)
        },
    ]);
    let over_plain = get_content_cell(&frame, 0);
    let over_watched = get_content_cell(&frame, 1);
    let mut client = build_test_client();
    let mut pending_mouse_actions = Vec::new();
    let mut forwarded_actions = Vec::new();

    // One pass holding more ticks than the cap. Ticks in alternating directions
    // never fold, so each one adds an action. Every hundredth lands on the pane
    // whose program reads the mouse, at its own column, and goes up as a report
    // instead of a scroll.
    for tick in 0..MAX_PENDING_MOUSE_ACTION_COUNT + 300 {
        let direction = if tick % 2 == 0 {
            ScrollDirection::Up
        } else {
            ScrollDirection::Down
        };
        if tick % 100 == 0 {
            let screen_point = Point {
                column: over_watched.column
                    + u16::try_from(tick / 100).expect("a handful of reports"),
                row: over_watched.row,
            };
            let mouse = build_mouse_wheel(direction, screen_point);
            handle_mouse_event(&mut client, &frame, mouse, &mut pending_mouse_actions);
            forwarded_actions.push(MouseAction::Forward {
                pane_id: watched,
                mouse_input: mouse,
            });
        } else {
            handle_mouse_event(
                &mut client,
                &frame,
                build_mouse_wheel(direction, over_plain),
                &mut pending_mouse_actions,
            );
        }
    }

    assert_eq!(
        pending_mouse_actions.len(),
        MAX_PENDING_MOUSE_ACTION_COUNT,
        "the pile stopped at the cap"
    );
    assert_eq!(
        forwarded_actions.len(),
        6,
        "six ticks landed on the watched pane"
    );
    let retained_forwarded_actions: Vec<MouseAction> = pending_mouse_actions
        .iter()
        .filter(|mouse_action| forwarded_actions.contains(mouse_action))
        .cloned()
        .collect();
    assert_eq!(
        retained_forwarded_actions, forwarded_actions,
        "every forwarded report is still in the pile, in the order it happened"
    );
}

#[test]
fn a_border_move_decided_before_the_answer_asks_only_for_what_the_answer_left() {
    let (mut client, frame, to) = build_border_drag_fixture();
    let further = Point {
        row: to.row + 1,
        ..to
    };
    let mut wire = build_test_wire();
    let mut sent = vec![build_sent_top_border_move(
        7,
        frame.mouse_panes[1].pane_id,
        3,
    )];
    let mut pending_mouse_actions = Vec::new();

    // One more cell of travel while round 7 is out. It is measured from the
    // anchor round 7 started position, so it names all four cells.
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(further),
        &mut pending_mouse_actions,
    );
    assert_eq!(
        pending_mouse_actions,
        vec![MouseAction::Resize {
            pane_id: frame.mouse_panes[1].pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 4,
        }]
    );

    apply_mouse_answers(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![build_top_border_resize_answer(
            frame.mouse_panes[1].pane_id,
            3,
        )],
        &mut pending_mouse_actions,
    );
    assert_eq!(sent, Vec::new(), "round 7 is answered and forgotten");
    flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);

    // Three of the four cells are already travelled, so the round that goes out
    // asks for the one the pointer is still ahead by.
    let request: IpcRequest = wire.session.recv().expect("read the next round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                pane_id: frame.mouse_panes[1].pane_id,
                border_side: Direction::Up,
                resize_step: -1,
                requested_cell_count: 1,
            }]),
        }
    );
}

#[test]
fn a_border_move_the_answer_covered_whole_leaves_nothing_to_send() {
    let (mut client, frame, to) = build_border_drag_fixture();
    let further = Point {
        row: to.row + 1,
        ..to
    };
    let mut wire = build_test_wire();
    let mut sent = vec![build_sent_top_border_move(
        7,
        frame.mouse_panes[1].pane_id,
        3,
    )];
    let mut pending_mouse_actions = Vec::new();

    // The pointer goes one cell further and comes straight back, so the newest
    // buffered move names exactly the three cells round 7 asked for.
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(further),
        &mut pending_mouse_actions,
    );
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(to),
        &mut pending_mouse_actions,
    );

    apply_mouse_answers(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![build_top_border_resize_answer(
            frame.mouse_panes[1].pane_id,
            3,
        )],
        &mut pending_mouse_actions,
    );
    flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);

    assert_eq!(sent, Vec::new(), "nothing was left to ask for");
    assert_eq!(pending_mouse_actions, Vec::new());
    // Nothing reached the wire: the sentinel is the first request read.
    let sentinel = wire.uplink.send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = wire.session.recv().expect("read the sentinel");
    assert_eq!(
        request,
        IpcRequest {
            request_id: sentinel,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn an_answer_that_lands_while_the_pointer_is_still_moves_the_drag_anchor() {
    let (mut client, frame, to) = build_border_drag_fixture();
    let pane_id = frame.mouse_panes[1].pane_id;
    let mut wire = build_test_wire();
    let mut sent = vec![build_sent_top_border_move(7, pane_id, 3)];
    let mut pending_mouse_actions = Vec::new();

    // A fourth cell of travel while round 7 is out, then round 7's answer: the
    // anchor takes the three cells the session moved and the one cell left over
    // goes out as the next round.
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(Point {
            row: to.row + 1,
            ..to
        }),
        &mut pending_mouse_actions,
    );
    apply_mouse_answers(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![build_top_border_resize_answer(
            frame.mouse_panes[1].pane_id,
            3,
        )],
        &mut pending_mouse_actions,
    );
    flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);

    let request: IpcRequest = wire.session.recv().expect("read the second round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                pane_id,
                border_side: Direction::Up,
                resize_step: -1,
                requested_cell_count: 1,
            }]),
        }
    );

    // The user pauses: the second round is answered with no drag event between
    // the flush and the answer, so nothing but the answer can move the anchor.
    apply_mouse_answers(
        &mut client,
        &frame,
        &mut sent,
        FIRST_POST_ATTACH_REQUEST_ID,
        vec![build_top_border_resize_answer(
            frame.mouse_panes[1].pane_id,
            1,
        )],
        &mut pending_mouse_actions,
    );
    assert_eq!(sent, Vec::new());
    assert_eq!(pending_mouse_actions, Vec::new());

    // One more cell of travel past the border it moved: the anchor sits on the
    // border's real row, so one cell of pointer travel asks for one cell.
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(Point {
            row: to.row + 2,
            ..to
        }),
        &mut pending_mouse_actions,
    );
    assert_eq!(
        pending_mouse_actions,
        vec![MouseAction::Resize {
            pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 1,
        }]
    );
}

#[test]
fn two_border_moves_in_one_round_each_land_on_their_own_border() {
    let (mut client, frame, to) = build_border_drag_fixture();
    let grabbed_pane_id = frame.mouse_panes[1].pane_id;
    let other_pane_id = frame.mouse_panes[0].pane_id;
    let further = Point {
        row: to.row + 1,
        ..to
    };
    let mut sent = vec![
        build_sent_top_border_move(7, grabbed_pane_id, 3),
        SentBorderMove {
            request_id: 7,
            pane_id: other_pane_id,
            border_side: Direction::Left,
            requested_cell_delta: 6,
        },
    ];
    // A move of another pane's left border, buffered before round 7 went out.
    let mut pending_mouse_actions = vec![MouseAction::Resize {
        pane_id: other_pane_id,
        border_side: Direction::Left,
        resize_step: 1,
        requested_cell_count: 6,
    }];

    // One more cell of travel on the grabbed border, so the pile holds one move
    // per border.
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(further),
        &mut pending_mouse_actions,
    );

    apply_mouse_answers(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![
            build_top_border_resize_answer(grabbed_pane_id, 3),
            MouseAnswer::Resized {
                pane_id: other_pane_id,
                border_side: Direction::Left,
                resize_step: 1,
                applied_cell_count: 1,
            },
        ],
        &mut pending_mouse_actions,
    );

    assert_eq!(sent, Vec::new(), "both of round 7's moves are answered");
    assert_eq!(
        pending_mouse_actions,
        vec![
            MouseAction::Resize {
                pane_id: other_pane_id,
                border_side: Direction::Left,
                resize_step: 1,
                requested_cell_count: 5,
            },
            MouseAction::Resize {
                pane_id: grabbed_pane_id,
                border_side: Direction::Up,
                resize_step: -1,
                requested_cell_count: 1,
            },
        ],
        "each buffered move lost only the cells its own border's answer took"
    );
    assert_eq!(
        client.handle_mouse(build_mouse_drag(further), &frame, Instant::now()),
        vec![MouseAction::Resize {
            pane_id: grabbed_pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 1,
        }],
        "the anchor took the three cells of the grabbed border's own answer, and no more"
    );
}

#[test]
fn two_moves_of_one_border_in_one_round_travel_the_newest_distance() {
    let pane_id = PaneId::new();
    let mut wire = build_test_wire();
    let mut sent = Vec::new();
    // A wheel tick between two moves of one border keeps them apart through the
    // fold. Both measure the whole travel from the same drag anchor: three cells
    // out, then five.
    let mut pending_mouse_actions = vec![
        MouseAction::Resize {
            pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        },
        MouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        },
        MouseAction::Resize {
            pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 5,
        },
    ];

    flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);

    assert_eq!(pending_mouse_actions, Vec::new());
    let request: IpcRequest = wire.session.recv().expect("read the round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(vec![
                WireMouseAction::Resize {
                    pane_id,
                    border_side: Direction::Up,
                    resize_step: -1,
                    requested_cell_count: 3,
                },
                WireMouseAction::Scroll {
                    pane_id,
                    is_scrolling_up: true,
                    scroll_line_count: 3,
                },
                WireMouseAction::Resize {
                    pane_id,
                    border_side: Direction::Up,
                    resize_step: -1,
                    requested_cell_count: 2,
                },
            ]),
        },
        "the session travels each move in turn, so the second asks for the two cells the first left"
    );
    assert_eq!(
        sent,
        vec![
            build_sent_top_border_move(FIRST_POST_ATTACH_REQUEST_ID, pane_id, 3),
            build_sent_top_border_move(FIRST_POST_ATTACH_REQUEST_ID, pane_id, 2),
        ],
        "five cells of border travel recorded for the round, not eight"
    );
}

#[test]
fn an_answer_for_a_border_drag_that_already_ended_changes_nothing() {
    let frame = build_mouse_frame(&[
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(PaneId::new()),
    ]);
    let first_pane_id = frame.mouse_panes[1].pane_id;
    let second_pane_id = frame.mouse_panes[2].pane_id;
    let upper = Point {
        column: 10,
        row: frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let lower = Point {
        column: 10,
        row: frame.session_snapshot.active_tab_snapshot.pane_slots[2]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let held = Point {
        row: lower.row + 1,
        ..lower
    };
    let mut client = build_test_client();
    let now = Instant::now();
    let mut sent = vec![build_sent_top_border_move(7, first_pane_id, 3)];
    let mut pending_mouse_actions = Vec::new();

    // Round 7 asks for three cells of the upper divider.
    client.handle_mouse(build_mouse_press(upper), &frame, now);
    assert_eq!(
        client.handle_mouse(
            build_mouse_drag(Point {
                row: upper.row + 3,
                ..upper
            }),
            &frame,
            now
        ),
        vec![MouseAction::Resize {
            pane_id: first_pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        }]
    );

    // The user lets go and grabs the lower divider instead, all while round 7
    // is still out.
    assert_eq!(
        client.handle_mouse(
            build_mouse_release(Point {
                row: upper.row + 3,
                ..upper
            }),
            &frame,
            now
        ),
        Vec::new()
    );
    client.handle_mouse(build_mouse_press(lower), &frame, now);
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(held),
        &mut pending_mouse_actions,
    );
    assert_eq!(
        pending_mouse_actions,
        vec![MouseAction::Resize {
            pane_id: second_pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 1,
        }]
    );

    apply_mouse_answers(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![build_top_border_resize_answer(first_pane_id, 3)],
        &mut pending_mouse_actions,
    );

    assert_eq!(sent, Vec::new(), "round 7's move is answered and forgotten");
    assert_eq!(
        pending_mouse_actions,
        vec![MouseAction::Resize {
            pane_id: second_pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 1,
        }],
        "the buffered move is for the other border, so the answer left it alone"
    );
    assert_eq!(
        client.handle_mouse(build_mouse_drag(held), &frame, Instant::now()),
        vec![MouseAction::Resize {
            pane_id: second_pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 1,
        }],
        "the new drag's anchor never moved, so the same pointer still asks for one cell"
    );
}

#[test]
fn border_moves_written_back_to_back_ask_only_for_the_cells_no_round_asked_yet() {
    let frame = build_mouse_frame(&[
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(PaneId::new()),
    ]);
    let pane_id = frame.mouse_panes[1].pane_id;
    let grabbed = get_border_divider_point(&frame);
    let mut client = build_test_client();
    let mut wire = build_test_wire();
    let mut sent = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    client.handle_mouse(build_mouse_press(grabbed), &frame, Instant::now());

    // The pointer walks three cells down, one cell per pass, with none of the
    // rounds answered. Each move names its whole distance from the drag anchor —
    // 1, then 2, then 3 — and each round asks for the one cell the rounds
    // already on the wire did not.
    for cell in 1..=3u16 {
        handle_mouse_event(
            &mut client,
            &frame,
            build_mouse_drag(Point {
                row: grabbed.row + cell,
                ..grabbed
            }),
            &mut pending_mouse_actions,
        );
        assert_eq!(
            pending_mouse_actions,
            vec![MouseAction::Resize {
                pane_id,
                border_side: Direction::Up,
                resize_step: -1,
                requested_cell_count: cell,
            }],
            "the anchor has not moved, so the whole distance is named again"
        );
        flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);
        let request: IpcRequest = wire.session.recv().expect("read the round");
        assert_eq!(
            request,
            IpcRequest {
                request_id: FIRST_POST_ATTACH_REQUEST_ID + u64::from(cell) - 1,
                request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                    pane_id,
                    border_side: Direction::Up,
                    resize_step: -1,
                    requested_cell_count: 1,
                }]),
            }
        );
    }
    assert_eq!(
        compute_sent_border_cell_delta(&sent, pane_id, Direction::Up),
        -3,
        "three cells asked for over three rounds, one each"
    );

    // The three answers come back, each taking its own round's cell off.
    for round in 0..3u64 {
        apply_mouse_answers(
            &mut client,
            &frame,
            &mut sent,
            FIRST_POST_ATTACH_REQUEST_ID + round,
            vec![build_top_border_resize_answer(pane_id, 1)],
            &mut pending_mouse_actions,
        );
    }

    assert_eq!(sent, Vec::new());
    assert_eq!(
        client.handle_mouse(
            build_mouse_drag(Point {
                row: grabbed.row + 3,
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
    let (mut client, frame, to) = build_border_drag_fixture();
    let pane_id = frame.mouse_panes[1].pane_id;
    let mut wire = build_test_wire();
    let mut sent = vec![build_sent_top_border_move(7, pane_id, 3)];
    let mut pending_mouse_actions = Vec::new();

    // Round 7 asked for three cells and the session took none: the border is
    // against a wall. The anchor stays put and the image record goes, so the pointer's
    // next move asks for its whole distance again.
    apply_mouse_answers(
        &mut client,
        &frame,
        &mut sent,
        7,
        vec![build_top_border_resize_answer(pane_id, 0)],
        &mut pending_mouse_actions,
    );
    assert_eq!(sent, Vec::new());

    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(to),
        &mut pending_mouse_actions,
    );
    flush_mouse_round(&mut wire.uplink, &mut sent, &mut pending_mouse_actions);

    let request: IpcRequest = wire.session.recv().expect("read the next round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                pane_id,
                border_side: Direction::Up,
                resize_step: -1,
                requested_cell_count: 3,
            }]),
        },
        "none of the three cells moved, so all three are asked for again"
    );
}

#[test]
fn a_key_the_keymap_does_not_bind_goes_up_the_connection_whole() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();

    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(KeyChord::from_parts(
                ModFlags::NONE,
                Key::Char('a'),
            )),
        },
    );

    // The sentinel goes out behind the key, so a key that was never sent reads
    // back as the sentinel instead of leaving this test waiting.
    wire.uplink.send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = wire.session.recv().expect("read the key");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Keyboard {
                key_input: crate::tests::build_key_input_for_chord(KeyChord::from_parts(
                    ModFlags::NONE,
                    Key::Char('a'),
                )),
            },
        }
    );
}

/// The release the outer terminal reports for one chord.
fn build_key_release_for_chord(chord: KeyChord) -> KeyInput {
    KeyInput {
        key_event_kind: KeyEventKind::Release,
        ..crate::tests::build_key_input_for_chord(chord)
    }
}

#[test]
fn a_key_release_goes_up_the_connection_and_leaves_an_open_sequence_open() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();
    // `<C-p>` is the default pane prefix: it binds nothing on its own, so it
    // opens a sequence and holds the keyboard.
    let sequence_opener_chord = KeyChord::from_parts(ModFlags::CTRL, Key::Char('p'));

    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(sequence_opener_chord),
        },
    );
    let chords_after_opener = client
        .get_pending_key_sequence()
        .expect("the opener left a sequence open")
        .list_chords()
        .to_vec();

    let released_key_input = build_key_release_for_chord(sequence_opener_chord);
    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: released_key_input.clone(),
        },
    );

    assert_eq!(
        client
            .get_pending_key_sequence()
            .expect("the sequence is still open")
            .list_chords(),
        chords_after_opener,
        "the release advanced no sequence"
    );

    // The opener was held, so the release is the first thing this connection
    // carries. The sentinel behind it keeps a missing release from waiting.
    wire.uplink.send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = wire.session.recv().expect("read the release");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Keyboard {
                key_input: released_key_input
            },
        }
    );
}

#[test]
fn a_key_no_binding_can_name_goes_up_the_connection_whole() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();
    // Left Shift reports as codepoint 57441 and has no chord form at all.
    let left_shift_key_input = KeyInput {
        key: KeyIdentity::Codepoint(57441),
        key_event_kind: KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
        modifier_flags: KeyModifierFlags::SHIFT,
    };

    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: left_shift_key_input.clone(),
        },
    );

    wire.uplink.send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = wire.session.recv().expect("read the key");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Keyboard {
                key_input: left_shift_key_input
            },
        }
    );
}

#[test]
fn a_text_only_event_goes_up_the_connection_whole() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();
    // A terminal that reports text with no key sends codepoint 0, and no
    // keybinding can name it.
    let text_only_key_input = KeyInput {
        key: KeyIdentity::Codepoint(TEXT_ONLY_KEY_CODEPOINT),
        key_event_kind: KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: "é".to_string(),
        modifier_flags: KeyModifierFlags::NONE,
    };

    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: text_only_key_input.clone(),
        },
    );

    wire.uplink.send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = wire.session.recv().expect("read the text-only event");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Keyboard {
                key_input: text_only_key_input
            },
        }
    );
}

#[test]
fn a_key_release_leaves_this_viewers_selection_gesture_running() {
    // The press that typed into the pane already ended the gesture. A release
    // arriving after a fresh press must not end the next one.
    let pane_id = PaneId::new();
    let frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();
    let mut pending_mouse_actions = Vec::new();

    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_press(get_content_cell(&frame, 0)),
        &mut pending_mouse_actions,
    );

    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: build_key_release_for_chord(KeyChord::from_parts(
                ModFlags::NONE,
                Key::Char('a'),
            )),
        },
    );

    pending_mouse_actions.clear();
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(Point { column: 9, row: 8 }),
        &mut pending_mouse_actions,
    );

    assert_eq!(
        pending_mouse_actions
            .last()
            .expect("the drag extended the gesture"),
        &MouseAction::Command(Command::Visual(VisualCommand::SetSelection(
            SetSelectionArgs {
                pane_id,
                selection: Selection {
                    selection_kind: SelectionKind::Character,
                    anchor: GridPosition {
                        row_index: 1,
                        column_index: 1
                    },
                    cursor: GridPosition {
                        row_index: 6,
                        column_index: 8
                    },
                },
            }
        )))
    );
}

#[test]
fn a_key_the_pane_gets_ends_this_viewers_selection_gesture() {
    // The key is the program's, so the highlight gesture over it is over. The
    // highlight it already made stands; only the drag ends.
    let pane_id = PaneId::new();
    let frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();
    let mut pending_mouse_actions = Vec::new();

    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_press(get_content_cell(&frame, 0)),
        &mut pending_mouse_actions,
    );
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(Point { column: 9, row: 8 }),
        &mut pending_mouse_actions,
    );
    assert_eq!(
        pending_mouse_actions
            .last()
            .expect("the drag decided something"),
        &MouseAction::Command(Command::Visual(VisualCommand::SetSelection(
            SetSelectionArgs {
                pane_id,
                selection: Selection {
                    selection_kind: SelectionKind::Character,
                    anchor: GridPosition {
                        row_index: 1,
                        column_index: 1
                    },
                    cursor: GridPosition {
                        row_index: 6,
                        column_index: 8
                    },
                },
            }
        )))
    );

    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(KeyChord::from_parts(
                ModFlags::NONE,
                Key::Char('a'),
            )),
        },
    );

    // The same move again, with no gesture left to extend.
    pending_mouse_actions.clear();
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(Point {
            column: 20,
            row: 12,
        }),
        &mut pending_mouse_actions,
    );
    assert_eq!(pending_mouse_actions, Vec::new());
}

#[test]
fn a_terminal_resize_moves_the_viewers_own_size_and_tells_the_session() {
    // The viewer hit-tests its own frames against this size, and the session
    // reconciles the tab size from every viewer's report, so both copies move.
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();
    let bigger = Size {
        column_count: 132,
        row_count: 43,
    };
    assert_eq!(client.get_viewport_size(), TEST_VIEWPORT_SIZE);
    let measurement = koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
        .expect("positive cell dimensions");

    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::Resize {
            client_id,
            viewport_size: bigger,
            pane_area: None,
            cell_size: Some(measurement),
        },
    );

    assert_eq!(
        client.get_viewport_size(),
        bigger,
        "the viewer's own copy moved"
    );
    // The sentinel goes out behind the report, so a report that was never sent
    // reads back as the sentinel instead of leaving this test waiting.
    wire.uplink.send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = wire.session.recv().expect("read the resize");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Resize {
                viewport: bigger,
                pane_area: Some(compute_core_pane_area(bigger)),
                cell_size: None,
            },
        }
    );
}

#[test]
fn a_terminal_resize_reports_its_measured_cell_size() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, true, false);
    let measurement = koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
        .expect("positive cell dimensions");

    process_runtime_input_with_cell_size(
        &mut client,
        &mut wire.uplink,
        &mut cell_size_query,
        RuntimeEvent::Resize {
            client_id,
            viewport_size: Size {
                column_count: 100,
                row_count: 40,
            },
            pane_area: None,
            cell_size: Some(measurement),
        },
    );

    wire.uplink.send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = wire.session.recv().expect("read the resize");
    let IpcRequestKind::Resize { cell_size, .. } = request.request_kind else {
        panic!("expected a Resize, got {:?}", request.request_kind);
    };
    assert_eq!(cell_size, Some(measurement));
}

#[test]
fn a_paste_goes_up_the_connection_as_the_text_the_terminal_delivered() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();

    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: String::from("hello\nworld"),
        },
    );

    // The sentinel goes out behind the paste, so a paste that was never sent
    // reads back as the sentinel instead of leaving this test waiting.
    wire.uplink.send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = wire.session.recv().expect("read the paste");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Paste {
                pasted_text: String::from("hello\nworld"),
            },
        },
        "the block crosses whole, line break and all"
    );
}

#[test]
fn a_paste_too_big_for_one_frame_is_the_only_thing_lost() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();

    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: "a".repeat(MAX_FRAME_BYTE_COUNT as usize + 1),
        },
    );
    wire.uplink.send_request(IpcRequestKind::Discovery);

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
            request_id: FIRST_POST_ATTACH_REQUEST_ID + 1,
            request_kind: IpcRequestKind::Discovery,
        },
        "the paste alone is dropped, not the connection"
    );
}

#[test]
fn a_paste_ends_this_viewers_selection_gesture() {
    let pane_id = PaneId::new();
    let frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut wire = build_test_wire();
    let mut pending_mouse_actions = Vec::new();

    // Press and drag: the gesture is under way, so the move names a highlight.
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_press(get_content_cell(&frame, 0)),
        &mut pending_mouse_actions,
    );
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(Point { column: 9, row: 8 }),
        &mut pending_mouse_actions,
    );
    assert_eq!(
        pending_mouse_actions
            .last()
            .expect("the drag decided something"),
        &MouseAction::Command(Command::Visual(VisualCommand::SetSelection(
            SetSelectionArgs {
                pane_id,
                selection: Selection {
                    selection_kind: SelectionKind::Character,
                    anchor: GridPosition {
                        row_index: 1,
                        column_index: 1
                    },
                    cursor: GridPosition {
                        row_index: 6,
                        column_index: 8
                    },
                },
            }
        )))
    );

    process_runtime_input(
        &mut client,
        &mut wire.uplink,
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: String::from("hello"),
        },
    );

    // The same move again, with no gesture left to extend.
    pending_mouse_actions.clear();
    handle_mouse_event(
        &mut client,
        &frame,
        build_mouse_drag(Point {
            column: 20,
            row: 12,
        }),
        &mut pending_mouse_actions,
    );
    assert_eq!(pending_mouse_actions, Vec::new());
}

#[test]
fn earliest_of_two_present_durations_is_the_smaller_either_order() {
    let short = Duration::from_millis(5);
    let long = Duration::from_millis(50);
    assert_eq!(
        select_earliest_duration(Some(short), Some(long)),
        Some(short)
    );
    assert_eq!(
        select_earliest_duration(Some(long), Some(short)),
        Some(short)
    );
}

#[test]
fn earliest_of_two_equal_durations_returns_that_duration() {
    let same = Duration::from_millis(10);
    assert_eq!(select_earliest_duration(Some(same), Some(same)), Some(same));
}

#[test]
fn earliest_falls_back_to_whichever_single_side_is_present() {
    let only = Duration::from_millis(7);
    assert_eq!(select_earliest_duration(Some(only), None), Some(only));
    assert_eq!(select_earliest_duration(None, Some(only)), Some(only));
}

#[test]
fn earliest_of_two_absent_durations_is_none() {
    assert_eq!(select_earliest_duration(None, None), None);
}

/// A backend that can reject the buffer draw while keeping the test terminal
/// at a fixed size.
struct FailingBackend {
    terminal_size: RatatuiSize,
    should_fail_draw: bool,
}

impl FailingBackend {
    fn from_terminal_size(terminal_size: Size) -> Self {
        FailingBackend {
            terminal_size: RatatuiSize {
                width: terminal_size.column_count,
                height: terminal_size.row_count,
            },
            should_fail_draw: false,
        }
    }
}

impl Backend for FailingBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, _content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        if self.should_fail_draw {
            return Err(io::Error::other("the test backend rejected the draw"));
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        Ok(Position::ORIGIN)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, _position: P) -> Result<(), Self::Error> {
        Ok(())
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn clear_region(&mut self, _clear_type: ClearType) -> Result<(), Self::Error> {
        Ok(())
    }

    fn size(&self) -> Result<RatatuiSize, Self::Error> {
        Ok(self.terminal_size)
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        Ok(WindowSize {
            columns_rows: self.terminal_size,
            pixels: RatatuiSize {
                width: 0,
                height: 0,
            },
        })
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A screen drawing into memory at [`TEST_VIEWPORT_SIZE`], 80 by 24 cells.
fn build_test_screen() -> Screen<TestBackend> {
    Screen::from_terminal_and_viewport(
        Terminal::new(TestBackend::new(
            TEST_VIEWPORT_SIZE.column_count,
            TEST_VIEWPORT_SIZE.row_count,
        ))
        .expect("build an in-memory terminal"),
        TEST_VIEWPORT_SIZE,
    )
}

fn build_native_image_snapshot() -> RenderSnapshot {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let image_record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Kitty,
        image: Arc::new(DecodedImage {
            pixel_width: 1,
            pixel_height: 1,
            rgba_bytes: vec![255, 0, 0, 255],
        }),
        animation: None,
        action: ImageAction::TransmitAndDisplay,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    });
    let placement = ImagePlacementSnapshot::from_image_record(1, image_record, (0, 0), 1, 1)
        .expect("the test image placement is valid");

    RenderSnapshot {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_revision: 0,
            session_name: String::from("session"),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: String::from("tab"),
                pane_slots: vec![PaneSlot {
                    pane_id,
                    outer_rect: Rect::from_origin_and_size(
                        Point { column: 0, row: 0 },
                        TEST_VIEWPORT_SIZE,
                    ),
                    content_rect: Some(Rect::from_origin_and_size(
                        Point { column: 0, row: 1 },
                        Size {
                            column_count: TEST_VIEWPORT_SIZE.column_count,
                            row_count: TEST_VIEWPORT_SIZE.row_count - 2,
                        },
                    )),
                    pane_kind: PaneKind::Terminal,
                    is_visible: true,
                    is_suppressed: false,
                    is_dead: false,
                }],
                effective_cell_size: TEST_VIEWPORT_SIZE,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                are_all_panes_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMeta {
                tab_id,
                tab_name: String::from("tab"),
                tab_index: 0,
                is_active: true,
            }],
        },
        pane_snapshots: vec![PaneSnapshot {
            pane_id,
            pane_title: None,
            cursor_snapshot: CursorSnapshot {
                row_index: 0,
                column_index: 0,
                is_visible: false,
                is_blinking: false,
                shape: None,
            },
            terminal_grid_view: Some(GridView {
                grid: Arc::new(Grid::blank(
                    TEST_VIEWPORT_SIZE.row_count - 2,
                    TEST_VIEWPORT_SIZE.column_count,
                    Style::default(),
                )),
                view_row_offset: 0,
            }),
            image_placement_snapshots: vec![placement],
            is_reverse_video: false,
            mouse_tracking: MouseTracking::Off,
            is_alternate_scroll_enabled: false,
            is_on_alternate_screen: false,
            view_top_row_index: 0,
            selection_spans: None,
            has_selection: false,
            scrollback_meta: ScrollbackMeta {
                is_truncated: false,
                retained_line_count: 0,
            },
        }],
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            client_revision: 0,
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
            lock_mode: LockMode::Normal,
            is_mouse_selection_enabled: false,
        },
        plugin_ui_snapshot: PluginUiSnapshot::default(),
    }
}

/// A screen whose backend can fail its next buffer draw.
fn build_failing_screen() -> Screen<FailingBackend> {
    Screen::from_terminal_and_viewport(
        Terminal::new(FailingBackend::from_terminal_size(TEST_VIEWPORT_SIZE))
            .expect("build a failing in-memory terminal"),
        TEST_VIEWPORT_SIZE,
    )
}

#[test]
fn a_screen_starts_with_the_compiled_in_region_solve() {
    let screen = build_test_screen();

    assert_eq!(screen.committed_regions.viewport_size, TEST_VIEWPORT_SIZE);
    assert_eq!(screen.committed_regions.region_input_revision, 0);
    assert_eq!(
        screen.committed_regions.solved_regions.pane_rect,
        Rect::from_origin_and_size(
            Point { column: 0, row: 1 },
            Size {
                column_count: TEST_VIEWPORT_SIZE.column_count,
                row_count: TEST_VIEWPORT_SIZE.row_count - 2,
            },
        )
    );
}

#[test]
fn a_new_viewport_commits_a_new_region_revision_with_the_build_test_painted_frame() {
    let mut screen = Screen::from_terminal_and_viewport(
        Terminal::new(TestBackend::new(100, 30)).expect("build an in-memory terminal"),
        TEST_VIEWPORT_SIZE,
    );
    let mut client = build_test_client();
    client.set_viewport(Size {
        column_count: 100,
        row_count: 30,
    });

    let frame = screen
        .draw_painted_frame(
            &mut client,
            Box::new(build_test_painted_frame_with_viewport(Size {
                column_count: 100,
                row_count: 30,
            })),
        )
        .expect("paint");

    assert_eq!(
        screen.committed_regions.viewport_size,
        Size {
            column_count: 100,
            row_count: 30
        }
    );
    assert_eq!(screen.committed_regions.region_input_revision, 1);
    assert_eq!(frame.committed_regions, screen.committed_regions);
}

#[test]
fn an_in_flight_frame_uses_its_own_viewport_for_region_geometry() {
    let mut screen = Screen::from_terminal_and_viewport(
        Terminal::new(TestBackend::new(120, 40)).expect("build an in-memory terminal"),
        TEST_VIEWPORT_SIZE,
    );
    let mut client = build_test_client();
    let resized = Size {
        column_count: 120,
        row_count: 40,
    };
    client.set_viewport(resized);

    let frame = screen
        .draw_painted_frame(&mut client, Box::new(build_test_painted_frame()))
        .expect("paint");
    let expected_committed_regions = CommittedRegions::core(TEST_VIEWPORT_SIZE, 0);

    assert_eq!(frame.committed_regions, expected_committed_regions);
    assert_eq!(screen.committed_regions, expected_committed_regions);
}

#[test]
fn a_failed_paint_keeps_the_visible_frame_and_viewer_state_paired() {
    let mut screen = build_failing_screen();
    let mut client = build_test_client();

    screen
        .draw_painted_frame(
            &mut client,
            Box::new(build_test_painted_frame_with_lock_mode(LockMode::Normal)),
        )
        .expect("the first frame paints");
    let shown_viewer_paint_before = screen.shown_viewer_paint.clone();
    let snapshot_before = screen.last_snapshot.clone();
    let regions_before = screen.committed_regions.clone();

    let opener = get_sequence_opener(&client);
    assert_eq!(
        client.resolve_key(opener, Instant::now()),
        KeyOutcome::Pending
    );

    let mut failed_frame = build_test_painted_frame_with_lock_mode(LockMode::Locked);
    failed_frame.session_snapshot.session_name = String::from("next");
    failed_frame.client_snapshot.is_mouse_selection_enabled = true;
    screen.terminal.backend_mut().should_fail_draw = true;

    assert_eq!(
        screen.draw_painted_frame(&mut client, Box::new(failed_frame)),
        None,
        "the backend rejects the frame"
    );
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert!(!client.is_mouse_selection_enabled());
    assert_eq!(
        client.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(opener))
    );
    assert_eq!(screen.shown_viewer_paint, shown_viewer_paint_before);
    assert_eq!(screen.last_snapshot, snapshot_before);
    assert_eq!(screen.committed_regions, regions_before);
    assert_eq!(screen.last_window_title, "session");

    screen.terminal.backend_mut().should_fail_draw = false;
    let mut next_frame = build_test_painted_frame_with_lock_mode(LockMode::Locked);
    next_frame.session_snapshot.session_name = String::from("next");
    next_frame.client_snapshot.is_mouse_selection_enabled = true;

    screen
        .draw_painted_frame(&mut client, Box::new(next_frame))
        .expect("the next paint succeeds");
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert!(client.is_mouse_selection_enabled());
    assert_eq!(client.get_pending_key_sequence(), None);
    assert_eq!(
        screen
            .shown_viewer_paint
            .as_ref()
            .map(|viewer_paint| viewer_paint.lock_mode),
        Some(LockMode::Locked)
    );
    assert_eq!(screen.last_window_title, "next");
}

#[test]
fn an_image_output_failure_keeps_the_committed_text_frame() {
    let error = io::Error::new(io::ErrorKind::BrokenPipe, "image output closed");
    let paint_result: Result<bool, terminal::PaintError<io::Error>> =
        Err(terminal::PaintError::Image(error));

    assert!(!is_frame_committed(paint_result));
}

#[test]
fn a_failed_native_frame_keeps_an_idle_retry_wakeup() {
    let mut screen = Screen::with_graphics_support(
        Terminal::new(TestBackend::new(
            TEST_VIEWPORT_SIZE.column_count,
            TEST_VIEWPORT_SIZE.row_count,
        ))
        .expect("build terminal"),
        TEST_VIEWPORT_SIZE,
        terminal::GraphicsSupport::Kitty,
        None,
    );
    screen.pending_snapshot = Some(build_native_image_snapshot());
    screen.image_output_state.fail_frame_commit();

    assert!(!screen.image_output_state.work_pending());
    assert_eq!(
        screen.next_image_wakeup(),
        Some(IMAGE_OUTPUT_STEP_DELAY_DURATION),
        "a retained snapshot must wake an idle attachment"
    );
}

#[test]
fn a_repeated_native_frame_failure_uses_capped_retry_delay() {
    let mut screen = Screen::with_graphics_support(
        Terminal::new(FailingBackend::from_terminal_size(TEST_VIEWPORT_SIZE))
            .expect("build terminal"),
        TEST_VIEWPORT_SIZE,
        terminal::GraphicsSupport::Kitty,
        None,
    );
    let mut client = build_test_client();
    let snapshot = build_native_image_snapshot();
    let active_tab_id = snapshot.client_snapshot.active_tab_id;
    screen.terminal.backend_mut().should_fail_draw = true;

    assert_eq!(screen.draw_snapshot(&mut client, snapshot), None);

    let deadline = Instant::now() + Duration::from_secs(5);
    while screen.image_output_state.work_pending() {
        screen.refresh(&mut client, Some(active_tab_id));
        assert!(
            Instant::now() < deadline,
            "native image preparation did not reach the failed commit"
        );
        std::thread::yield_now();
    }
    assert!(screen.pending_snapshot.is_some());
    assert_eq!(screen.native_retry_delay, Duration::from_millis(2));

    let mut expected_delay = Duration::from_millis(2);
    while expected_delay < MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION {
        let retry_at = screen
            .native_retry_at
            .expect("a failed commit has a deadline");
        assert_eq!(
            screen.next_image_wakeup_at(retry_at - expected_delay),
            Some(expected_delay)
        );
        let refresh_outcome = screen.refresh_at(
            &mut client,
            Some(active_tab_id),
            retry_at - Duration::from_nanos(1),
        );
        assert_eq!(
            refresh_outcome, None,
            "input events before the retry deadline must not retry the frame"
        );
        assert!(!screen.image_output_state.work_pending());
        assert_eq!(screen.native_retry_delay, expected_delay);
        loop {
            let retry_at = screen.native_retry_at.expect("the retry remains scheduled");
            screen.refresh_at(&mut client, Some(active_tab_id), retry_at);
            assert!(
                Instant::now() < deadline,
                "native image retry did not reach the next failed commit"
            );
            if screen.native_retry_delay > expected_delay {
                break;
            }
            std::thread::yield_now();
        }
        expected_delay = expected_delay
            .saturating_mul(2)
            .min(MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION);
    }

    let retry_at = screen
        .native_retry_at
        .expect("the capped retry has a deadline");
    assert_eq!(
        screen.next_image_wakeup_at(retry_at - MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION),
        Some(MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION)
    );
    screen.refresh_at(&mut client, Some(active_tab_id), retry_at);
    while screen.image_output_state.work_pending() {
        let retry_at = screen.native_retry_at.expect("the retry remains scheduled");
        screen.refresh_at(&mut client, Some(active_tab_id), retry_at);
        assert!(
            Instant::now() < deadline,
            "the capped native image retry did not finish"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        screen.native_retry_delay,
        MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION
    );

    screen.terminal.backend_mut().should_fail_draw = false;
    while screen.pending_snapshot.is_some() {
        let retry_at = screen.native_retry_at.expect("the retry remains scheduled");
        screen.refresh_at(&mut client, Some(active_tab_id), retry_at);
        assert!(
            Instant::now() < deadline,
            "native image output did not recover after the writer recovered"
        );
        std::thread::yield_now();
    }
    assert_eq!(screen.native_retry_delay, IMAGE_OUTPUT_STEP_DELAY_DURATION);
}

#[test]
fn a_native_retry_delay_resets_when_output_connection_resets() {
    let mut screen = Screen::with_graphics_support(
        Terminal::new(TestBackend::new(
            TEST_VIEWPORT_SIZE.column_count,
            TEST_VIEWPORT_SIZE.row_count,
        ))
        .expect("build terminal"),
        TEST_VIEWPORT_SIZE,
        terminal::GraphicsSupport::Kitty,
        None,
    );
    screen.native_retry_delay = MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION;
    screen.native_retry_at = Some(Instant::now());

    screen.reset_connection();

    assert_eq!(screen.native_retry_delay, IMAGE_OUTPUT_STEP_DELAY_DURATION);
    assert_eq!(screen.native_retry_at, None);
}

#[test]
fn a_pending_text_frame_does_not_schedule_an_image_wakeup() {
    let mut screen = build_test_screen();
    screen.pending_snapshot = Some(build_render_snapshot(&build_test_painted_frame()));

    assert_eq!(screen.next_image_wakeup(), None);
}

#[test]
fn a_completed_native_image_is_committed_by_refresh() {
    let mut screen = Screen::with_graphics_support(
        Terminal::new(TestBackend::new(
            TEST_VIEWPORT_SIZE.column_count,
            TEST_VIEWPORT_SIZE.row_count,
        ))
        .expect("build terminal"),
        TEST_VIEWPORT_SIZE,
        terminal::GraphicsSupport::Kitty,
        None,
    );
    let mut client = build_test_client();
    let snapshot = build_native_image_snapshot();
    let active_tab_id = snapshot.client_snapshot.active_tab_id;

    assert_eq!(
        screen.draw_snapshot(&mut client, snapshot),
        None,
        "the first paint waits for native image preparation"
    );
    assert!(screen.pending_snapshot.is_some());
    assert!(screen.next_image_wakeup().is_some());

    let deadline = Instant::now() + Duration::from_secs(5);
    while screen.pending_snapshot.is_some() {
        screen.refresh(&mut client, Some(active_tab_id));
        assert!(
            Instant::now() < deadline,
            "native image preparation did not reach refresh"
        );
        std::thread::yield_now();
    }

    assert!(screen.last_snapshot.is_some());
    assert_eq!(screen.next_image_wakeup(), None);
}

#[test]
fn native_images_return_after_scrolling_away_for_each_output_protocol() {
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let output_protocols = [
        terminal::GraphicsSupport::Kitty,
        terminal::GraphicsSupport::Iterm,
        terminal::GraphicsSupport::Sixel {
            palette_color_count: 256,
            max_pixel_width: None,
            max_pixel_height: None,
        },
    ];

    for graphics in output_protocols {
        let mut screen = Screen::with_graphics_support(
            Terminal::new(TestBackend::new(
                TEST_VIEWPORT_SIZE.column_count,
                TEST_VIEWPORT_SIZE.row_count,
            ))
            .expect("build terminal"),
            TEST_VIEWPORT_SIZE,
            graphics,
            Some(cell_size),
        );
        let mut client = build_test_client();
        let image = build_native_image_snapshot();
        let active_tab_id = image.client_snapshot.active_tab_id;
        let mut scrolled_away = image.clone();
        for pane_snapshot in &mut scrolled_away.pane_snapshots {
            pane_snapshot.image_placement_snapshots.clear();
        }

        assert_eq!(
            screen.draw_snapshot(&mut client, image.clone()),
            None,
            "{graphics:?} must wait for its first native output"
        );
        wait_for_pending_native_frame(&mut screen, &mut client, active_tab_id);
        assert_eq!(
            screen
                .image_output_state
                .list_prepared_placement_keys()
                .len(),
            1,
            "{graphics:?}"
        );

        assert!(
            screen.draw_snapshot(&mut client, scrolled_away).is_some(),
            "{graphics:?} must commit the frame with no visible image"
        );
        assert!(
            screen
                .image_output_state
                .list_prepared_placement_keys()
                .is_empty(),
            "{graphics:?}"
        );

        assert_eq!(
            screen.draw_snapshot(&mut client, image),
            None,
            "{graphics:?} must prepare the image again after it returns"
        );
        wait_for_pending_native_frame(&mut screen, &mut client, active_tab_id);
        assert_eq!(
            screen
                .image_output_state
                .list_prepared_placement_keys()
                .len(),
            1,
            "{graphics:?}"
        );
    }
}

fn wait_for_pending_native_frame(
    screen: &mut Screen<TestBackend>,
    client: &mut Client,
    active_tab_id: TabId,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while screen.pending_snapshot.is_some() {
        screen.refresh(client, Some(active_tab_id));
        assert!(
            Instant::now() < deadline,
            "native image preparation did not reach refresh"
        );
        std::thread::yield_now();
    }
    assert_eq!(screen.next_image_wakeup(), None);
}

#[test]
fn a_viewer_only_refresh_waits_for_a_frame_after_resize() {
    let mut screen = build_test_screen();
    let mut client = build_test_client();
    let painted_frame = build_test_painted_frame();
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;
    screen
        .draw_painted_frame(&mut client, Box::new(painted_frame))
        .expect("paint");
    let terminal_buffer_before_refresh = screen.terminal.backend().buffer().clone();

    client.set_viewport(Size {
        column_count: 100,
        row_count: 30,
    });
    client.set_reconnecting(Some(Reconnecting {
        attempt: 1,
        retry_in_seconds: 1,
    }));
    screen.refresh(&mut client, Some(active_tab_id));

    assert_eq!(
        screen.terminal.backend().buffer(),
        &terminal_buffer_before_refresh
    );
    assert_eq!(screen.committed_regions.viewport_size, TEST_VIEWPORT_SIZE);
}

/// The hint bar of what the screen last drew: the bottom row, trailing blanks
/// removed.
fn get_hint_row(screen: &Screen<TestBackend>) -> String {
    let buffer = screen.terminal.backend().buffer();
    let column_count = buffer.area.width as usize;
    buffer
        .content()
        .chunks(column_count)
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
fn get_sequence_opener(client: &Client) -> KeyChord {
    client
        .build_frame_hints(client.get_lock_mode(), false)
        .hint_bindings
        .iter()
        .find(|hint_binding| hint_binding.key_sequence.list_chords().len() > 1)
        .expect("the built-in normal mode binds at least one multi-chord sequence")
        .key_sequence
        .list_chords()[0]
}

/// The pointer moved to `at` with no button held.
fn build_mouse_motion(screen_point: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Motion,
        position: screen_point,
        modifier_flags: ModFlags::NONE,
    }
}

#[test]
fn the_frame_that_locks_the_client_draws_the_locked_hint_bar() {
    // The first draw leaves the viewer in normal mode. The second is the frame
    // that locks it, and its own paint lists the locked bindings.
    let mut client = build_test_client();
    let mut screen = build_test_screen();

    screen
        .draw_painted_frame(
            &mut client,
            Box::new(build_test_painted_frame_with_lock_mode(LockMode::Normal)),
        )
        .expect("paint");
    let normal = get_hint_row(&screen);

    screen
        .draw_painted_frame(
            &mut client,
            Box::new(build_test_painted_frame_with_lock_mode(LockMode::Locked)),
        )
        .expect("paint");
    let locked = get_hint_row(&screen);

    assert_eq!(locked, " Ctrl +  l  Unlock  g  Mouse Select  q  Quit");
    assert_ne!(normal, locked);
}

#[test]
fn a_frame_moves_the_viewer_to_the_mode_it_reports() {
    let mut client = build_test_client();
    assert_eq!(client.get_lock_mode(), LockMode::Normal);

    apply_frame_to_client(
        &mut client,
        &build_render_snapshot(&build_test_painted_frame_with_lock_mode(LockMode::Locked)),
    );

    assert_eq!(client.get_lock_mode(), LockMode::Locked);
}

#[test]
fn a_prefix_key_typed_after_a_frame_in_one_pass_still_draws_its_breadcrumb() {
    // One drained batch can carry a frame and a keypress together. The frame is
    // drawn first, and the key opens a sequence after it, so the end of the pass
    // has to draw again.
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let painted_frame = build_test_painted_frame_with_lock_mode(LockMode::Normal);
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;

    screen
        .draw_painted_frame(&mut client, Box::new(painted_frame))
        .expect("paint");
    let drawn = get_hint_row(&screen);

    let opener = get_sequence_opener(&client);
    assert_eq!(
        client.resolve_key(opener, Instant::now()),
        KeyOutcome::Pending
    );
    screen.refresh(&mut client, Some(active_tab_id));

    // The breadcrumb view opens with the chord that was pressed and ends with
    // the arrow that separates it from the chords continuing it.
    let breadcrumb = get_hint_row(&screen);
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
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let painted = build_test_painted_frame_with_lock_mode(LockMode::Normal);
    let active_tab_id = painted.client_snapshot.active_tab_id;
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mouse = build_mouse_frame(&[
        build_plain_mouse_pane(first_pane_id),
        build_plain_mouse_pane(second_pane_id),
    ]);
    let mut pending_mouse_actions = Vec::new();

    screen
        .draw_painted_frame(&mut client, Box::new(painted))
        .expect("paint");
    let hovered_before = ViewerPaint::from_client_and_tab(&client, active_tab_id)
        .chrome
        .hovered_pane_id;

    handle_mouse_event(
        &mut client,
        &mouse,
        build_mouse_motion(get_content_cell(&mouse, 1)),
        &mut pending_mouse_actions,
    );
    let hovered_after = ViewerPaint::from_client_and_tab(&client, active_tab_id)
        .chrome
        .hovered_pane_id;
    screen.refresh(&mut client, Some(active_tab_id));

    assert_eq!(hovered_before, None);
    assert_eq!(hovered_after, Some(second_pane_id));
    assert_eq!(
        screen
            .shown_viewer_paint
            .as_ref()
            .map(|viewer_paint| viewer_paint.chrome.hovered_pane_id),
        Some(Some(second_pane_id)),
        "the screen records the hover it just drew"
    );
}

#[test]
fn a_pass_that_moved_nothing_draws_nothing() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let painted_frame = build_test_painted_frame_with_lock_mode(LockMode::Normal);
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;

    screen
        .draw_painted_frame(&mut client, Box::new(painted_frame))
        .expect("paint");
    let drawn = screen.terminal.backend().buffer().clone();
    let shown_viewer_paint = screen.shown_viewer_paint.clone();

    screen.refresh(&mut client, Some(active_tab_id));

    assert_eq!(*screen.terminal.backend().buffer(), drawn);
    assert_eq!(screen.shown_viewer_paint, shown_viewer_paint);
}

#[test]
fn a_screen_with_no_frame_yet_draws_nothing() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();

    screen.refresh(&mut client, None);
    screen.refresh(&mut client, Some(TabId::new()));

    assert_eq!(screen.shown_viewer_paint, None);
    assert_eq!(screen.last_snapshot, None);
}

#[test]
fn locking_the_client_moves_what_the_viewer_paints() {
    let mut client = build_test_client();
    let painted_frame = build_test_painted_frame_with_lock_mode(LockMode::Locked);
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;
    let paint_before_lock = ViewerPaint::from_client_and_tab(&client, active_tab_id);

    apply_frame_to_client(&mut client, &build_render_snapshot(&painted_frame));

    let paint_after_lock = ViewerPaint::from_client_and_tab(&client, active_tab_id);
    assert_eq!(paint_before_lock.lock_mode, LockMode::Normal);
    assert_eq!(paint_after_lock.lock_mode, LockMode::Locked);
    assert_ne!(paint_after_lock, paint_before_lock);
}

#[test]
fn opening_a_key_sequence_moves_what_the_viewer_paints() {
    // An opened sequence reaches no session, so no frame comes back. The hint
    // bar draws it as a breadcrumb, so `ViewerPaint` carries it.
    let mut client = build_test_client();
    let active_tab_id = TabId::new();
    let opener = get_sequence_opener(&client);
    let paint_before_key_sequence = ViewerPaint::from_client_and_tab(&client, active_tab_id);

    assert_eq!(
        client.resolve_key(opener, Instant::now()),
        KeyOutcome::Pending
    );

    let paint_after_key_sequence = ViewerPaint::from_client_and_tab(&client, active_tab_id);
    assert_eq!(paint_before_key_sequence.pending_key_sequence, None);
    assert_eq!(
        paint_after_key_sequence.pending_key_sequence,
        Some(KeySequence::from(opener))
    );
    assert_ne!(paint_after_key_sequence, paint_before_key_sequence);
}

#[test]
fn dialing_again_moves_what_the_viewer_paints() {
    // No frame arrives while the link is down, so the `RECONNECTING` tag is
    // drawn by the repaint a moved `ViewerPaint` fires.
    let mut client = build_test_client();
    let active_tab_id = TabId::new();
    let paint_before_reconnect = ViewerPaint::from_client_and_tab(&client, active_tab_id);

    let dialing = Reconnecting {
        attempt: 1,
        retry_in_seconds: 5,
    };
    client.set_reconnecting(Some(dialing));

    let paint_after_reconnect = ViewerPaint::from_client_and_tab(&client, active_tab_id);
    assert_eq!(paint_before_reconnect.chrome.reconnecting, None);
    assert_eq!(paint_after_reconnect.chrome.reconnecting, Some(dialing));
    assert_ne!(paint_after_reconnect, paint_before_reconnect);
}

#[test]
fn every_event_from_the_blackout_is_dropped_and_the_hangup_still_reported() {
    let client = build_test_client();
    let sequence_opener_chord = get_sequence_opener(&client);
    let resized = Size {
        column_count: 100,
        row_count: 40,
    };
    let (incoming_tx, incoming_rx) = build_incoming_channel();
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    incoming_tx
        .send(Incoming::Input(Box::new(RuntimeEvent::KeyInput {
            client_id: client.get_client_id(),
            key_input: crate::tests::build_key_input_for_chord(sequence_opener_chord),
        })))
        .expect("the loop's channel takes it");
    incoming_tx
        .send(Incoming::Input(Box::new(RuntimeEvent::Quit)))
        .expect("the loop's channel takes it");
    incoming_tx
        .send(Incoming::Input(Box::new(RuntimeEvent::Resize {
            client_id: client.get_client_id(),
            viewport_size: resized,
            pane_area: None,
            cell_size: None,
        })))
        .expect("the loop's channel takes it");
    incoming_tx
        .send(Incoming::Frame {
            connection_index: 0,
            session_event_result: Ok(SessionEvent::Painted {
                frame: Box::new(build_test_painted_frame()),
            }),
        })
        .expect("the loop's channel takes it");

    let viewport_before_blackout = client.get_viewport_size();

    assert!(
        drop_input_from_the_blackout(&incoming_rx, &mut cell_size_query),
        "the terminal hung up while the link was down"
    );

    assert_eq!(
        client.get_viewport_size(),
        viewport_before_blackout,
        "the resize was dropped with the rest; the new link reads the size itself"
    );
    assert_ne!(
        viewport_before_blackout, resized,
        "the dropped resize named another size"
    );
    assert_eq!(
        incoming_rx.try_recv().err(),
        Some(mpsc::TryRecvError::Empty),
        "the key, the resize and the stale frame were all taken off the channel"
    );
}

#[test]
fn a_cell_size_reply_in_the_blackout_is_consumed_by_the_outer_query() {
    let measurement = koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
        .expect("positive cell dimensions");
    let client_id = ClientId::new();
    let (incoming_tx, incoming_rx) = build_incoming_channel();
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, true, true);
    incoming_tx
        .send(Incoming::Input(Box::new(RuntimeEvent::CellSize {
            client_id,
            cell_size: measurement,
        })))
        .expect("the cell-size reply is queued");

    assert!(!drop_input_from_the_blackout(
        &incoming_rx,
        &mut cell_size_query,
    ));
    assert_eq!(
        cell_size_query.accept_cell_size_reply(measurement),
        (None, false),
        "the blackout consumed the reply instead of leaving it pending"
    );
}

#[test]
fn incoming_batch_leaves_events_past_the_per_pass_limit_queued() {
    let (incoming_tx, incoming_rx) = mpsc::channel();
    for _ in 0..MAX_INCOMING_EVENT_COUNT_PER_PASS {
        incoming_tx
            .send(Incoming::Input(Box::new(RuntimeEvent::Quit)))
            .expect("the test receiver is open");
    }
    let client_id = ClientId::new();
    incoming_tx
        .send(Incoming::Input(Box::new(RuntimeEvent::Resize {
            client_id,
            viewport_size: Size {
                column_count: 90,
                row_count: 30,
            },
            pane_area: None,
            cell_size: None,
        })))
        .expect("the test receiver is open");

    let (incoming_events, deferred) = build_incoming_batch(None, &incoming_rx);

    assert_eq!(incoming_events.len(), MAX_INCOMING_EVENT_COUNT_PER_PASS);
    let None = deferred else {
        panic!("a full event-count batch must leave the next event in the channel");
    };
    for incoming_event in incoming_events {
        let Incoming::Input(runtime_event) = incoming_event else {
            panic!("the batch contained a session event");
        };
        match *runtime_event {
            RuntimeEvent::Quit => {}
            unexpected_runtime_event => panic!("the batch contained {unexpected_runtime_event:?}"),
        }
    }
    let remaining = incoming_rx
        .try_recv()
        .expect("the event past the count limit remains queued");
    let Incoming::Input(runtime_event) = remaining else {
        panic!("the remaining event came from the session");
    };
    match *runtime_event {
        RuntimeEvent::Resize {
            client_id: actual_client_id,
            viewport_size,
            pane_area,
            cell_size,
        } => {
            assert_eq!(actual_client_id, client_id);
            assert_eq!(
                viewport_size,
                Size {
                    column_count: 90,
                    row_count: 30
                }
            );
            assert_eq!(pane_area, None);
            assert_eq!(cell_size, None);
        }
        unexpected_runtime_event => panic!("the remaining event was {unexpected_runtime_event:?}"),
    }
    assert_eq!(
        incoming_rx.try_recv().err(),
        Some(mpsc::TryRecvError::Empty)
    );
}

#[test]
fn incoming_queue_stops_producers_at_its_fixed_capacity() {
    assert_eq!(INCOMING_QUEUE_CAPACITY, 16);
    let (incoming_tx, incoming_rx) = build_incoming_channel();
    for _ in 0..INCOMING_QUEUE_CAPACITY {
        incoming_tx
            .try_send(Incoming::Input(Box::new(RuntimeEvent::Quit)))
            .expect("an event inside the queue limit fits");
    }
    let client_id = ClientId::new();
    let rejected = incoming_tx.try_send(Incoming::Input(Box::new(RuntimeEvent::Resize {
        client_id,
        viewport_size: Size {
            column_count: 90,
            row_count: 30,
        },
        pane_area: None,
        cell_size: None,
    })));

    let Err(mpsc::TrySendError::Full(Incoming::Input(runtime_event))) = rejected else {
        panic!("the event past the queue limit was not returned as full");
    };
    let RuntimeEvent::Resize {
        client_id: actual_client_id,
        viewport_size,
        pane_area,
        cell_size,
    } = *runtime_event
    else {
        panic!("the full queue returned another event");
    };
    assert_eq!(actual_client_id, client_id);
    assert_eq!(
        viewport_size,
        Size {
            column_count: 90,
            row_count: 30
        }
    );
    assert_eq!(pane_area, None);
    assert_eq!(cell_size, None);
    for _ in 0..INCOMING_QUEUE_CAPACITY {
        let Incoming::Input(runtime_event) = incoming_rx
            .try_recv()
            .expect("each accepted event remains queued")
        else {
            panic!("the queue contained a session event");
        };
        let RuntimeEvent::Quit = *runtime_event else {
            panic!("the queue contained another input event");
        };
    }
    assert_eq!(
        incoming_rx.try_recv().err(),
        Some(mpsc::TryRecvError::Empty)
    );
}

#[test]
fn terminal_input_queue_stops_the_reader_at_its_fixed_capacity() {
    assert_eq!(INCOMING_QUEUE_CAPACITY, 16);
    let (input_tx, input_rx) = build_input_channel();
    for _ in 0..INCOMING_QUEUE_CAPACITY {
        input_tx
            .try_send(RuntimeEvent::Quit)
            .expect("an input event inside the queue limit fits");
    }
    let client_id = ClientId::new();
    let resized = Size {
        column_count: 90,
        row_count: 30,
    };
    let rejected = input_tx.try_send(RuntimeEvent::Resize {
        client_id,
        viewport_size: resized,
        pane_area: None,
        cell_size: None,
    });

    let Err(mpsc::TrySendError::Full(RuntimeEvent::Resize {
        client_id: actual_client_id,
        viewport_size,
        pane_area,
        cell_size,
    })) = rejected
    else {
        panic!("the input event past the queue limit was not returned as full");
    };
    assert_eq!(actual_client_id, client_id);
    assert_eq!(viewport_size, resized);
    assert_eq!(pane_area, None);
    assert_eq!(cell_size, None);
    for _ in 0..INCOMING_QUEUE_CAPACITY {
        let runtime_event = input_rx
            .try_recv()
            .expect("each accepted input remains queued");
        let RuntimeEvent::Quit = runtime_event else {
            panic!("the input queue contained another event");
        };
    }
    assert_eq!(input_rx.try_recv().err(), Some(mpsc::TryRecvError::Empty));
}

#[test]
fn incoming_batch_defers_an_image_chunk_past_the_byte_limit() {
    let (incoming_tx, incoming_rx) = mpsc::channel();
    let chunk_len = MAX_INCOMING_IMAGE_BYTE_COUNT_PER_BATCH / 2 + 1;
    let first_chunk = koshi_ipc::frame::FrameImageChunk {
        image_transfer_id: 7,
        byte_offset: 0,
        is_last: false,
        chunk_bytes: vec![1; chunk_len],
    };
    let second_chunk = koshi_ipc::frame::FrameImageChunk {
        image_transfer_id: 7,
        byte_offset: u64::try_from(chunk_len).expect("the chunk length fits an offset"),
        is_last: true,
        chunk_bytes: vec![2; chunk_len],
    };
    incoming_tx
        .send(Incoming::Frame {
            connection_index: 0,
            session_event_result: Ok(SessionEvent::ImageContentChunk {
                image_chunk: first_chunk.clone(),
            }),
        })
        .expect("the first image chunk is queued");
    incoming_tx
        .send(Incoming::Frame {
            connection_index: 0,
            session_event_result: Ok(SessionEvent::ImageContentChunk {
                image_chunk: second_chunk.clone(),
            }),
        })
        .expect("the second image chunk is queued");
    let first_incoming_frame = incoming_rx
        .try_recv()
        .expect("the first image chunk is available");

    let (incoming_events, deferred) =
        build_incoming_batch(Some(first_incoming_frame), &incoming_rx);

    assert_eq!(incoming_events.len(), 1);
    let mut incoming_event_iterator = incoming_events.into_iter();
    let Some(Incoming::Frame {
        connection_index,
        session_event_result: Ok(SessionEvent::ImageContentChunk { image_chunk }),
    }) = incoming_event_iterator.next()
    else {
        panic!("the batch did not contain the first image chunk");
    };
    assert_eq!(connection_index, 0);
    assert_eq!(image_chunk, first_chunk);
    assert_eq!(incoming_event_iterator.next().map(|_| ()), None);
    let Some(Incoming::Frame {
        connection_index,
        session_event_result: Ok(SessionEvent::ImageContentChunk { image_chunk }),
    }) = deferred
    else {
        panic!("the second image chunk was not deferred");
    };
    assert_eq!(connection_index, 0);
    assert_eq!(image_chunk, second_chunk);
    assert_eq!(
        incoming_rx.try_recv().err(),
        Some(mpsc::TryRecvError::Empty)
    );
}

#[test]
fn a_new_connection_is_told_the_size_the_terminal_is_now() {
    let mut client = build_test_client();
    let (requests, sent) = mpsc::channel();
    let mut uplink = Uplink {
        request_sender: requests,
        registry: ActionRegistry::new(),
        next_request_id: FIRST_POST_ATTACH_REQUEST_ID,
    };

    report_terminal_size(&mut client, &mut uplink);

    let request = sent.try_recv().expect("the size was reported");
    assert_eq!(request.request_id, FIRST_POST_ATTACH_REQUEST_ID);
    let IpcRequestKind::Resize {
        viewport: viewport_size,
        pane_area,
        cell_size,
    } = request.request_kind
    else {
        panic!("expected a Resize, got {:?}", request.request_kind);
    };
    assert_eq!(
        viewport_size,
        client.get_viewport_size(),
        "the session is told the size the viewer holds"
    );
    assert_eq!(
        pane_area,
        Some(compute_core_pane_area(client.get_viewport_size())),
        "this client reports the pane area left by the built-in rows"
    );
    assert_eq!(cell_size, None);
    assert_eq!(
        sent.try_recv().err(),
        Some(mpsc::TryRecvError::Empty),
        "the size is reported once"
    );
}

#[test]
fn a_new_connection_reports_a_measured_cell_size() {
    let mut client = build_test_client();
    let (requests, sent) = mpsc::channel();
    let mut uplink = Uplink {
        request_sender: requests,
        registry: ActionRegistry::new(),
        next_request_id: FIRST_POST_ATTACH_REQUEST_ID,
    };
    let measurement = koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
        .expect("positive cell dimensions");
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, true, false);

    report_terminal_size_with_cell_size(
        &mut client,
        &mut uplink,
        &mut cell_size_query,
        Some(measurement),
    );

    let request = sent.try_recv().expect("the size was reported");
    let IpcRequestKind::Resize { cell_size, .. } = request.request_kind else {
        panic!("expected a Resize, got {:?}", request.request_kind);
    };
    assert_eq!(cell_size, Some(measurement));
}

#[test]
fn the_redial_wait_doubles_to_eight_seconds_and_holds_there() {
    let initial_redial_wait = FIRST_REDIAL_WAIT_DURATION;
    let doubled_redial_wait = next_redial_wait(initial_redial_wait);
    let quadrupled_redial_wait = next_redial_wait(doubled_redial_wait);
    let capped_redial_wait = next_redial_wait(quadrupled_redial_wait);
    let repeated_capped_redial_wait = next_redial_wait(capped_redial_wait);

    assert_eq!(initial_redial_wait, Duration::from_secs(1));
    assert_eq!(doubled_redial_wait, Duration::from_secs(2));
    assert_eq!(quadrupled_redial_wait, Duration::from_secs(4));
    assert_eq!(capped_redial_wait, Duration::from_secs(8));
    assert_eq!(repeated_capped_redial_wait, Duration::from_secs(8));
}

#[test]
fn a_pause_ending_on_or_past_the_window_is_not_taken() {
    // The first pause always fits, so the ladder always dials at least once.
    assert!(does_redial_pause_fit(
        Duration::ZERO,
        FIRST_REDIAL_WAIT_DURATION
    ));

    // 111 + 8 lands at 119 and fits; 112 + 8 lands exactly on 120 and does not.
    assert!(does_redial_pause_fit(
        Duration::from_secs(111),
        Duration::from_secs(8)
    ));
    assert!(!does_redial_pause_fit(
        Duration::from_secs(112),
        Duration::from_secs(8)
    ));

    // A pause that would end long past the window is refused too.
    assert!(!does_redial_pause_fit(
        REDIAL_WINDOW_DURATION,
        FIRST_REDIAL_WAIT_DURATION
    ));
}

#[test]
fn every_pause_the_ladder_hands_out_stays_inside_the_window() {
    // Walking the real ladder from the first pause, the loop stops before any
    // pause crosses 120 seconds, and it stops after taking at least one.
    let mut elapsed = Duration::ZERO;
    let mut wait = FIRST_REDIAL_WAIT_DURATION;
    let mut pauses = 0;
    while does_redial_pause_fit(elapsed, wait) {
        elapsed += wait;
        wait = next_redial_wait(wait);
        pauses += 1;
    }

    assert_eq!(pauses, 17);
    assert_eq!(elapsed, Duration::from_secs(119));
    assert!(elapsed < REDIAL_WINDOW_DURATION);
}

#[test]
fn a_pass_that_moves_nothing_leaves_what_the_viewer_paints_alone() {
    // The repaint fires on a difference, so two reads of one unchanged viewer
    // are equal.
    let client = build_test_client();
    let active_tab_id = TabId::new();

    assert_eq!(
        ViewerPaint::from_client_and_tab(&client, active_tab_id),
        ViewerPaint::from_client_and_tab(&client, active_tab_id)
    );
}

#[test]
fn a_deadline_already_past_still_takes_a_session_that_is_already_back() {
    // The client sits out its own wait for the frame that told it, so the
    // window can be spent by the time the wait runs. The file is read once
    // before the deadline is weighed, so a session that came back inside the
    // window is still joined.
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let advertised = write_advertised_endpoint(
        runtime_directory.path(),
        session_id,
        NEW_CONNECTION_TOKEN,
        4321,
    );

    assert_eq!(
        wait_for_new_endpoint(
            runtime_directory.path(),
            session_id,
            &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
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
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    std::fs::write(
        EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), session_id),
        b"{",
    )
    .expect("write a half endpoint file");

    let deadline = Instant::now() + Duration::from_millis(200);
    assert_eq!(
        wait_for_new_endpoint(
            runtime_directory.path(),
            session_id,
            &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
            deadline,
        ),
        None
    );
    assert!(
        Instant::now() >= deadline,
        "the wait sat out its whole window rather than giving up on the first read"
    );
}

/// How long a test waits for the reader thread to forward a frame before it
/// fails instead of hanging the suite.
const FRAME_READ_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

/// A live event stream: the session end that writes frames, and both halves of
/// the client end. The reading half is what the frame-reader thread owns; the
/// writing half is held so the connection stands exactly as it does in the
/// running loop.
///
/// The accept runs on its own thread, since the connect and the accept must
/// both be live for the connection to open.
fn build_test_event_stream() -> (Connection, FrameReader, FrameWriter) {
    // The address is a socket-file path on Unix and a bare pipe name on
    // Windows, so the directory goes unused there. `/tmp` keeps the path short
    // enough for the platform's socket-path limit; the session id makes it
    // unique, so tests running side by side never share one.
    #[cfg(unix)]
    let socket_directory = std::path::PathBuf::from("/tmp");
    #[cfg(windows)]
    let socket_directory = std::env::temp_dir();
    let socket_address = compute_socket_address(&socket_directory, SessionId::new());
    let listener = Listener::bind(&socket_address).expect("bind the event stream");
    let accept_thread = thread::spawn(move || listener.accept().expect("accept the connection"));
    let client_connection =
        Connection::connect(&socket_address).expect("connect to the event stream");
    let session_connection = accept_thread.join().expect("the accepting thread finished");
    let (frame_reader, frame_writer) = client_connection.split();
    (session_connection, frame_reader, frame_writer)
}

#[test]
fn a_frame_this_build_has_no_name_for_is_stepped_over_and_the_next_one_arrives() {
    // A newer session writes frames this build was compiled without. Ending the
    // read at the first one would drop the connection and report the session
    // dead, so the reader steps over it and forwards the frame after it.
    let (mut session, reader, _uplink) = build_test_event_stream();
    let (incoming_tx, incoming_rx) = build_incoming_channel();
    spawn_frame_reader(reader, 4, incoming_tx);

    let closed = SessionEvent::TabClosed {
        tab_id: TabId::new(),
    };
    session
        .send(&serde_json::json!({ "Floating": { "pane_id": 1 } }))
        .expect("write a frame this build has no name for");
    session
        .send(&closed)
        .expect("write the frame after it, which this build has");

    let Incoming::Frame {
        connection_index,
        session_event_result,
    } = incoming_rx
        .recv_timeout(FRAME_READ_TIMEOUT_DURATION)
        .expect("the reader forwarded a frame")
    else {
        panic!("the reader forwards frames, never terminal input");
    };
    assert_eq!(
        connection_index, 4,
        "the frame names the connection it was read on"
    );
    assert_eq!(
        session_event_result.expect("the frame after the unknown one decoded"),
        closed,
        "the frame after the one this build cannot name is the first the loop is handed",
    );
}

/// A session to switch to, as the command a plan carries.
fn build_switch_command() -> Command {
    Command::SwitchSession(SwitchSessionArgs {
        client_id: None,
        session_id: SessionId::new(),
    })
}

/// A plan that hands an action to a plugin. `core:lock` is only the name it
/// carries; nothing here runs it.
fn build_plugin_host_call() -> DispatchPlan {
    DispatchPlan::PluginHostCall {
        plugin_id: PluginId::new(),
        action_reference: ActionReference::from_core_action_name("lock")
            .expect("`lock` is a legal core action name"),
        action_arguments: ActionArgs::None,
    }
}

#[test]
fn a_sequence_plan_flattens_into_its_commands_in_the_order_they_run() {
    let first_switch_command = build_switch_command();
    let second_switch_command = build_switch_command();
    let third_switch_command = build_switch_command();
    let plan = DispatchPlan::Sequence(vec![
        DispatchPlan::Command(first_switch_command.clone()),
        DispatchPlan::Sequence(vec![
            DispatchPlan::Command(second_switch_command.clone()),
            DispatchPlan::Command(third_switch_command.clone()),
        ]),
    ]);

    assert_eq!(
        build_commands(plan),
        vec![
            first_switch_command,
            second_switch_command,
            third_switch_command
        ]
    );
}

#[test]
fn a_plugin_host_call_sends_no_command_from_this_side() {
    // The plugin host runs on the session, so this side has nothing to send.
    assert_eq!(
        build_commands(build_plugin_host_call()),
        Vec::<Command>::new()
    );
}

#[test]
fn a_sequence_holding_a_plugin_call_sends_only_the_commands_around_it() {
    let command_before_plugin = build_switch_command();
    let command_after_plugin = build_switch_command();
    let plan = DispatchPlan::Sequence(vec![
        DispatchPlan::Command(command_before_plugin.clone()),
        build_plugin_host_call(),
        DispatchPlan::Command(command_after_plugin.clone()),
    ]);

    assert_eq!(
        build_commands(plan),
        vec![command_before_plugin, command_after_plugin]
    );
}

#[test]
fn an_empty_sequence_plan_sends_nothing() {
    assert_eq!(
        build_commands(DispatchPlan::Sequence(Vec::new())),
        Vec::<Command>::new()
    );
}

#[test]
fn a_session_id_reads_as_an_id_and_every_other_value_as_a_display_name() {
    let session_id = SessionId::new();

    assert_eq!(
        build_session_selector(&session_id.to_string()),
        SessionSelector::SessionId(session_id),
        "a `session-<uuid>` value names the id"
    );
    assert_eq!(
        build_session_selector(&session_id.get_uuid().to_string()),
        SessionSelector::SessionId(session_id),
        "a bare UUID names the same id"
    );
    assert_eq!(
        build_session_selector("quiet-lake"),
        SessionSelector::SessionName(String::from("quiet-lake"))
    );
    assert_eq!(
        build_session_selector(""),
        SessionSelector::SessionName(String::new()),
        "an empty value is a display name the far side matches nothing against"
    );
    assert_eq!(
        build_session_selector("session-not-a-uuid"),
        SessionSelector::SessionName(String::from("session-not-a-uuid")),
        "the `session-` prefix alone does not make a value an id"
    );
}

#[test]
fn a_selector_reads_in_a_message_as_its_id_or_its_display_name() {
    let session_id = SessionId::new();

    assert_eq!(
        format_session_selector_name(&SessionSelector::SessionId(session_id)),
        session_id.to_string()
    );
    assert_eq!(
        format_session_selector_name(&SessionSelector::SessionName(String::from("quiet-lake"))),
        "quiet-lake"
    );
}

#[test]
fn a_number_too_large_to_parse_is_refused_like_any_other_line() {
    let session_rows = vec![build_test_session_row("a")];
    let error = parse_session_selection(&session_rows, "99999999999999999999999999")
        .expect_err("a number past the end of `usize` names no listed row");

    assert_eq!(
        error.to_string(),
        "invalid arguments: `99999999999999999999999999` is not one of the listed \
         sessions; expected a number 1 to 1"
    );
    assert_eq!(CliExitCode::from(&error), CliExitCode::UsageOrConfig);
}

#[test]
fn spaces_around_the_typed_number_still_pick_that_row() {
    let session_rows = vec![build_test_session_row("a"), build_test_session_row("b")];

    assert_eq!(
        parse_session_selection(&session_rows, "  2  \n").expect("the trimmed line names row 2"),
        1
    );
}
