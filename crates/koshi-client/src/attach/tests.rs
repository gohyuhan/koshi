//! Tests for the attached client: which session a bare `koshi attach` settles
//! on from a listing, that joining by id asks the router nothing, how one frame
//! read from the event stream decides whether the loop keeps reading and, when
//! it does not, how it ended, and the mouse path — what the pile folds to, what
//! one round writes, that a write never waits for an answer or for the socket,
//! and what an answer applies when it does come back. It also covers what a
//! paste sends, what it ends, what a paste too big for one frame costs, the
//! picker that chooses how long the loop may sleep, and what one frame draws:
//! the mode it moves the viewer to, the hint bar it lists that mode's bindings
//! in, and what a pass must move before the loop draws again on its own. It
//! also covers coming back after the session replaces its own process image:
//! that the join names the client record this terminal holds, and every way
//! back that fails reporting the death a broken connection already reports, and
//! a first dial that a session refuses for the token it advertised before,
//! which joins once the session advertises its own. It also covers the stretch
//! with no link: the redial countdown and its window, what a restart holds and
//! sends after the join, what a redial drops, that the quit keybinding leaves
//! at once with a link and without one, that a lock key switches the mode the
//! next key resolves in before the session answers, with a link and during a
//! restart, a restarted session that never answers the join, and what a viewer
//! that stopped dialing or left while reconnecting prints and exits with. It
//! also covers how a typed value reads as a session id or as a display name,
//! and that a termination signal puts a quit on the loop's channel and is
//! recorded unless this process ignores it. These tests also check that an
//! accepted Enter swap updates the shown pane rectangles without a resize and
//! that placement messages describe both panes inside the preview.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Size as RatatuiSize};

use koshi_config::layer::PartialKeybindingsConfig;
use koshi_config::types::{KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::command::{
    ClearSelectionArgs, CliExitCode, CloseTabArgs, FocusTabArgs, GridPosition, NewTabArgs,
    PanePlacementAnchor, PanePlacementTarget, Selection, SelectionKind, SetSelectionArgs,
    TabTarget, TiledPlacementTarget, VisualCommand,
};
use koshi_core::constant::{DEFAULT_MAX_FRAMES_PER_SECOND, MIN_LINK_TIMEOUT_MS};
use koshi_core::geometry::{Direction, PaneArea, Point, Rect, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, TabId};
use koshi_core::key::{
    BindingModifierFlags, Key, KeyChord, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags,
    NamedKey, TEXT_ONLY_KEY_CODEPOINT,
};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseAnswer, MouseButton, MouseKind, MouseTracking, ScrollDirection};
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::endpoint::{compute_socket_address, EndpointFile};
use koshi_ipc::frame::{
    FrameClient, FrameFloatingPaneSlot, FrameGraphicsProtocol, FrameImageAction, FrameImageDisplay,
    FrameImageRecordHeader, FrameImageTransfer, FrameSession, FrameSlot, FrameTab, PaintedFrame,
};
use koshi_ipc::protocol::{
    ConnectionToken, IpcErrorCode, IpcErrorPayload, IpcResponse, IpcResult, ProcessImageId,
};
use koshi_ipc::remote_servers::SavedServer;
use koshi_ipc::remote_wire::RemoteClientFrame;
use koshi_ipc::router::{
    compute_router_socket_address, resolve_router_endpoint_path, RouterRequest, RouterResponse,
    ROUTER_PROTOCOL_VERSION,
};
use koshi_ipc::transport::{
    build_frame_halves, Connection, Deadlined, Listener, MAX_FRAME_BYTE_COUNT,
};
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_renderer::snapshot::{
    ClientSnapshot, CommittedRegions, CursorSnapshot, FloatingPaneMoveDraft, FloatingPaneSlot,
    GridView, ImagePlacementSnapshot, MousePane, PaneSlot, PaneSnapshot, PlacementSnapshot,
    PlacementSource, RenderSnapshot, ScrollbackMetadata, SessionSnapshot, TabMetadata, TabSnapshot,
};
use koshi_renderer::{resolve_hit_region, HitRegion};
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord,
};
use koshi_terminal::grid::state::Grid;
use koshi_terminal::style::Style;

use super::*;
use crate::attach::paint::build_render_snapshot;
use crate::mouse::tests::build_plain_mouse_pane;
use crate::tests::{
    build_core_bound_action, build_test_client, float_placement_source_pane,
    get_source_tab_snapshot_mut, NO_ROOM_TAB_SIZE, TEST_VIEWPORT_SIZE,
};
use crate::{
    DockUndockMode, FloatingPaneMoveSelection, PaneMoveMode, PlacementMode, PlacementModeLifetime,
};
use koshi_runtime::runtime::painted_update::{PaintedUpdateSender, ReceivedPaintedFrame};
use koshi_test_support::fixtures::{
    build_accepted_hello_result, build_image_chunk_frame, build_restarting_refusal,
    build_test_runtime_directory, connect_test_socket_pair,
    spawn_session_listening_before_it_advertises, spawn_session_replacing_its_image,
    write_router_endpoint_file, write_session_endpoint_file,
};

impl<B: Backend> Screen<B> {
    /// A screen over `backend` that has drawn nothing yet, with no native
    /// image output and no cell measurement.
    fn from_backend_and_viewport_size(backend: B, viewport_size: Size) -> Self {
        Self::from_backend_with_graphics_support(
            backend,
            viewport_size,
            terminal::GraphicsSupport::Unsupported,
            None,
        )
    }

    /// A screen over `backend` that has drawn nothing yet, with
    /// `graphics_support` and `cell_size`. Its paint driver starts at
    /// `viewport_size`, and the frame bytes the backend does not write go
    /// nowhere.
    fn from_backend_with_graphics_support(
        backend: B,
        viewport_size: Size,
        graphics_support: terminal::GraphicsSupport,
        cell_size: Option<PixelCellSize>,
    ) -> Self {
        Self::with_graphics_support(
            terminal::PaintDriver::from_backend(backend, viewport_size),
            terminal::TerminalOutput::from_terminal_sink(Box::new(io::sink())),
            viewport_size,
            graphics_support,
            cell_size,
            Arc::new(|| {}),
        )
    }

    /// Build the render snapshot `painted_frame` carries and draw it with
    /// [`Screen::draw_snapshot`].
    fn draw_painted_frame(
        &mut self,
        client: &mut Client,
        painted_frame: Box<PaintedFrame>,
    ) -> Option<MouseFrame> {
        self.draw_painted_frame_with_placement_preview(client, painted_frame, None)
    }

    /// [`Self::draw_painted_frame`] of a frame that carries
    /// `placement_preview`.
    fn draw_painted_frame_with_placement_preview(
        &mut self,
        client: &mut Client,
        painted_frame: Box<PaintedFrame>,
        placement_preview: Option<Arc<PlacementPreview>>,
    ) -> Option<MouseFrame> {
        let mut render_snapshot =
            build_render_snapshot(&build_received_painted_frame(*painted_frame));
        render_snapshot.placement_preview = placement_preview;
        self.draw_snapshot(client, render_snapshot)
    }

    /// [`Screen::compute_next_image_wakeup_at`] at the current instant.
    fn compute_next_image_wakeup(&self) -> Option<Duration> {
        self.compute_next_image_wakeup_at(Instant::now())
    }
}

/// [`report_terminal_size_with_cell_size`] with no cell measurement in hand.
fn report_terminal_size(client: &mut Client, uplink: &mut Uplink) {
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    report_terminal_size_with_cell_size(client, uplink, &mut cell_size_query, None);
}

/// [`handle_terminal_input_event`] with a fresh terminal query state, no
/// painted frame and no held mouse actions, for `runtime_event` read at
/// `read_at`, an event that does not fire the quit binding.
fn process_runtime_input(
    client: &mut Client,
    uplink: &mut Uplink,
    runtime_event: RuntimeEvent,
    read_at: Instant,
) {
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    assert_eq!(
        handle_terminal_input_event(
            client,
            uplink,
            &mut cell_size_query,
            None,
            TerminalInput {
                runtime_event,
                read_at,
            },
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        ControlFlow::Continue(())
    );
}

/// `runtime_event` on the attachment loop's channel, read from this terminal
/// at `read_at`.
fn build_input_incoming(runtime_event: RuntimeEvent, read_at: Instant) -> Incoming {
    Incoming::Input(Box::new(TerminalInput {
        runtime_event,
        read_at,
    }))
}

/// The smallest frame a session can paint: one empty tab, no panes, the client
/// unlocked.
fn build_test_painted_frame() -> PaintedFrame {
    build_test_painted_frame_with_lock_mode(LockMode::Normal)
}

/// A normal painted frame carrying `viewport_size` in both its client and active-tab
/// data.
fn build_test_painted_frame_with_viewport_size(viewport_size: Size) -> PaintedFrame {
    let mut painted_frame = build_test_painted_frame();
    painted_frame.session_snapshot.active_tab_snapshot.tab_size = viewport_size;
    painted_frame.client_snapshot.viewport_size = viewport_size;
    painted_frame
}

/// The same frame, reporting the client in `lock_mode`. Each call mints a new
/// session id and a new tab id.
fn build_test_painted_frame_with_lock_mode(lock_mode: LockMode) -> PaintedFrame {
    let active_tab_id = TabId::new();
    PaintedFrame {
        is_recovery_notice_visible: false,
        session_snapshot: FrameSession {
            session_id: SessionId::new(),
            session_name: String::from("session"),
            active_tab_snapshot: FrameTab {
                tab_id: active_tab_id,
                tab_name: String::from("tab"),
                pane_slots: Vec::new(),
                tab_size: Size {
                    column_count: 80,
                    row_count: 24,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tab_snapshots: Vec::new(),
            floating_pane_slots: Vec::new(),
            floating_pane_ids: Vec::new(),
        },
        pane_snapshots: Vec::new(),
        client_snapshot: FrameClient {
            client_id: ClientId::new(),
            viewport_size: Size {
                column_count: 80,
                row_count: 24,
            },
            active_tab_id,
            focused_pane_id: None,
            active_tab_focused_pane_id: None,
            lock_mode,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            last_applied_placement_command_id: None,
            is_mouse_selection_enabled: false,
        },
    }
}

/// The applied first painted update of a connection whose JSON part is
/// `painted_frame`, a frame that holds no pane.
///
/// # Panics
/// Panics when `painted_frame` holds a pane.
fn build_received_painted_frame(painted_frame: PaintedFrame) -> ReceivedPaintedFrame {
    assert_eq!(
        painted_frame.pane_snapshots.len(),
        0,
        "a pane's rows travel in the update beside the frame"
    );
    ReceivedPaintedFrame {
        update_sequence: 1,
        painted_frame: Arc::new(painted_frame),
        placement_preview: None,
        pane_views: Vec::new(),
    }
}

/// The applied first painted update of a connection whose JSON part is
/// `painted_frame`, a frame that holds no pane, and whose placement preview is
/// `placement_preview`.
fn build_received_painted_frame_with_placement_preview(
    painted_frame: PaintedFrame,
    placement_preview: PlacementPreview,
) -> ReceivedPaintedFrame {
    let mut render_snapshot =
        build_render_snapshot(&build_received_painted_frame(painted_frame.clone()));
    render_snapshot.placement_preview = Some(Arc::new(placement_preview));
    crate::attach::paint::tests::carry_first_painted_frame(&render_snapshot, &painted_frame)
}

/// One listing row for a session displayed as `session_name`, on this machine.
fn build_test_session_row(session_name: &str) -> SessionRow {
    SessionRow {
        session_id: SessionId::new(),
        session_name: String::from(session_name),
        server_name_or_address: None,
        owner_user_id: None,
    }
}

#[test]
fn no_running_session_leaves_nothing_to_attach_to() {
    let selection_error =
        parse_session_selection(&[], "").expect_err("an empty listing has no session");
    assert_eq!(selection_error.to_string(), "no koshi session is running");
    assert_eq!(
        CliExitCode::from(&selection_error),
        CliExitCode::SessionNotFound
    );
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
    // `select_session` sends a lone remote row through `parse_session_selection`:
    // an empty line refuses instead of settling on the row.
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
            owner_user_id: None,
        },
        SessionRow {
            session_id: shared_session_id,
            session_name: String::from("web"),
            server_name_or_address: Some(String::from("desk-ip")),
            owner_user_id: None,
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
        let selection_error = parse_session_selection(&session_rows, typed_input)
            .expect_err("the line names no listed row");
        assert_eq!(
            selection_error.to_string(),
            format!(
                "invalid arguments: `{typed_input}` is not one of the listed sessions; \
                 expected a number 1 to 3"
            )
        );
        assert_eq!(
            CliExitCode::from(&selection_error),
            CliExitCode::UsageOrConfig
        );
    }
}

/// A stand-in router that records the name of every request it is asked. It
/// answers a Hello with the settled protocol version and refuses every other
/// request with `BadToken`.
///
/// It serves one connection and records each request before it answers it. A
/// caller that has its answer finds its request already in the returned list.
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
            let router_result = match request.request_kind {
                RouterRequestKind::Hello { .. } => RouterResult::Hello {
                    protocol_version: ROUTER_PROTOCOL_VERSION,
                    build_version: String::new(),
                    process_image_id: Some(ProcessImageId::get_own()),
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
                answer_result: router_result,
            });
        }
    });
    request_kind_names
}

#[test]
fn attaching_by_id_skips_the_selector_lookup() {
    let runtime_directory = build_test_runtime_directory();
    let recorded_request_kind_names = build_recording_router(runtime_directory.path());
    let session_id = SessionId::new();

    let attach_error = attach_session(runtime_directory.path(), None, session_id)
        .expect_err("no endpoint file advertises that session");

    let request_kind_names = recorded_request_kind_names
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
    let CliError::SessionNotFound { session_name } = attach_error else {
        panic!("expected SessionNotFound, got {attach_error:?}");
    };
    assert_eq!(session_name, session_id.to_string());
}

/// A lookup by display name dials the router: the stand-in answers the Hello,
/// records both requests, and its refusal of the lookup comes back as the
/// error.
#[test]
fn looking_up_a_name_asks_the_selector_lookup() {
    let runtime_directory = build_test_runtime_directory();
    let recorded_request_kind_names = build_recording_router(runtime_directory.path());

    let lookup_error = lookup_session_address(runtime_directory.path(), "quiet-lake")
        .expect_err("the stand-in router refuses the lookup");

    let request_kind_names = recorded_request_kind_names
        .lock()
        .expect("the list outlives every panic")
        .clone();
    assert_eq!(request_kind_names, vec!["Hello", "AttachLookup"]);
    let CliError::IpcUnavailable { detail } = lookup_error else {
        panic!("expected IpcUnavailable, got {lookup_error:?}");
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
fn the_switch_frame_ends_the_stream_with_the_session_and_user_to_join_next() {
    let session_id = SessionId::new();
    assert_eq!(
        classify_session_event(&Ok(SessionEvent::SwitchTo {
            session_id,
            owner_user_id: Some(1001),
        })),
        Some(AttachmentEnding::SwitchSession {
            session_id,
            owner_user_id: Some(1001),
        })
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
            tab_id: Some(tab_id),
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
            tab_id: Some(tab_id),
        },
        SessionEvent::PaneFocused {
            client_id: ClientId::new(),
            tab_id: Some(tab_id),
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
        owner_user_id: None,
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
    let ending_error = report_attachment_ending(
        &build_local_home(),
        AttachmentEnding::ConnectionDied,
        session_id,
        None,
    )
    .expect_err("a death is an error");
    assert_eq!(
        ending_error.to_string(),
        format!(
            "the session ended unexpectedly\n  \
             run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&ending_error), CliExitCode::RuntimeAction);
}

#[test]
fn a_death_on_another_machine_reports_the_way_back_to_that_machine() {
    // The way back names the saved server `work`, not this machine.
    let session_id = SessionId::new();
    let ending_error = report_attachment_ending(
        &build_remote_home(),
        AttachmentEnding::ConnectionDied,
        session_id,
        None,
    )
    .expect_err("a death is an error");
    assert_eq!(
        ending_error.to_string(),
        format!(
            "the session ended unexpectedly\n  \
             run `koshi attach --remote work` to see that server's sessions; \
             if session {session_id} is among them, reattach with \
             `koshi attach --remote work {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&ending_error), CliExitCode::RuntimeAction);
}

#[test]
fn a_server_with_no_name_is_named_by_its_address_in_the_way_back() {
    let session_id = SessionId::new();
    let remote_home = Home::Remote {
        server: ServerReference::New {
            server_address: "laptop.local:7654".to_string(),
            certificate_fingerprint: "ab".repeat(32),
        },
    };
    let ending_error = report_attachment_ending(
        &remote_home,
        AttachmentEnding::ConnectionDied,
        session_id,
        None,
    )
    .expect_err("a death is an error");
    assert_eq!(
        ending_error.to_string(),
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
    let ending_error = report_attachment_ending(
        &build_remote_home(),
        AttachmentEnding::LinkLost(Box::new(cause)),
        session_id,
        None,
    )
    .expect_err("a lost link is an error");
    assert_eq!(
        ending_error.to_string(),
        format!(
            "the token this server saved does not reach session 7\n  \
             the session continues without you\n  \
             run `koshi attach --remote work` to see that server's sessions; \
             if session {session_id} is among them, reattach with \
             `koshi attach --remote work {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&ending_error), CliExitCode::RuntimeAction);
}

#[test]
fn two_lost_links_are_equal_only_when_their_causes_print_the_same_text() {
    let build_lost_link = |detail: &str| {
        AttachmentEnding::LinkLost(Box::new(CliError::Runtime {
            detail: detail.to_string(),
        }))
    };

    assert_eq!(
        build_lost_link("connection refused"),
        build_lost_link("connection refused")
    );
    assert_ne!(
        build_lost_link("connection refused"),
        build_lost_link("connection timed out")
    );
    assert_ne!(
        build_lost_link("connection refused"),
        AttachmentEnding::ConnectionDied
    );
    assert_ne!(
        AttachmentEnding::ConnectionDied,
        build_lost_link("connection refused")
    );
}

#[test]
fn two_refusals_after_a_restart_are_equal_only_when_their_refusals_print_the_same_text() {
    let build_refusal_after_restart = |detail: &str| {
        AttachmentEnding::RefusedAfterRestart(Box::new(CliError::IpcUnavailable {
            detail: detail.to_string(),
        }))
    };

    assert_eq!(
        build_refusal_after_restart("this session holds no such client"),
        build_refusal_after_restart("this session holds no such client")
    );
    assert_ne!(
        build_refusal_after_restart("this session holds no such client"),
        build_refusal_after_restart("this session speaks protocol version 9")
    );
    assert_ne!(
        build_refusal_after_restart("this session holds no such client"),
        AttachmentEnding::Restarting
    );
    assert_ne!(
        build_refusal_after_restart("this session holds no such client"),
        AttachmentEnding::LinkLost(Box::new(CliError::IpcUnavailable {
            detail: "this session holds no such client".to_string(),
        }))
    );
}

#[test]
fn a_link_answering_in_the_format_of_koshi_0_4_0_is_a_refused_dial() {
    let dial_error = build_link_failure(IpcError::PreviousReleaseAnswer);

    let DialError::Refused(CliError::PreviousReleaseServer { detail }) = dial_error else {
        panic!("expected a refused dial, got {dial_error:?}");
    };
    assert_eq!(
        detail,
        "the server answered in the format of koshi 0.4.0 or older, which this koshi cannot \
         talk to"
    );
}

#[test]
fn a_link_that_breaks_is_an_unreachable_dial() {
    let dial_error = build_link_failure(IpcError::Disconnected);

    let DialError::Unreachable(CliError::IpcUnavailable { detail }) = dial_error else {
        panic!("expected an unreachable dial, got {dial_error:?}");
    };
    assert_eq!(detail, "ipc peer disconnected");
}

#[test]
fn a_refused_redial_ends_the_stretch_with_no_link_with_that_refusal() {
    let redial_answer = build_redial_answer(Err(DialError::Refused(CliError::Runtime {
        detail: "the server host:7654 did not admit the connection".to_string(),
    })));

    let RejoinAnswer::Ended(attachment_ending) = redial_answer else {
        panic!("a refused dial ends the redial");
    };
    assert_eq!(
        attachment_ending,
        AttachmentEnding::LinkLost(Box::new(CliError::Runtime {
            detail: "the server host:7654 did not admit the connection".to_string(),
        }))
    );
}

#[test]
fn an_unreachable_or_restarting_server_answers_a_redial_as_unreachable() {
    let unreachable_answer =
        build_redial_answer(Err(DialError::Unreachable(CliError::IpcUnavailable {
            detail: "connection refused".to_string(),
        })));
    let restarting_answer =
        build_redial_answer(Err(DialError::Restarting(CliError::IpcUnavailable {
            detail: "the router is restarting".to_string(),
        })));

    let RejoinAnswer::Unreachable(CliError::IpcUnavailable {
        detail: unreachable_detail,
    }) = unreachable_answer
    else {
        panic!("an unreachable server is dialed again");
    };
    assert_eq!(unreachable_detail, "connection refused");
    let RejoinAnswer::Unreachable(CliError::IpcUnavailable {
        detail: restarting_detail,
    }) = restarting_answer
    else {
        panic!("a restarting server is dialed again");
    };
    assert_eq!(restarting_detail, "the router is restarting");
}

#[test]
fn a_restarted_remote_session_that_refuses_this_protocol_version_ends_the_redial_at_once_with_that_refusal(
) {
    let mut dial_count = 0_u32;
    let redial_result = redial_restarted_remote_session_with(
        || {
            dial_count += 1;
            Err(DialError::Refused(CliError::ProtocolVersionRefused {
                detail: "this session speaks protocol version 9".to_string(),
            }))
        },
        Instant::now() + RESTART_WINDOW_DURATION,
    );

    assert_eq!(
        redial_result.map(|joined| joined.client_id),
        Err(AttachmentEnding::RefusedAfterRestart(Box::new(
            CliError::ProtocolVersionRefused {
                detail: "this session speaks protocol version 9".to_string(),
            }
        )))
    );
    assert_eq!(dial_count, 1);
}

#[test]
fn a_restarted_remote_session_that_refuses_for_another_reason_is_dialed_again() {
    let mut dial_count = 0_u32;
    let redial_result = redial_restarted_remote_session_with(
        || {
            dial_count += 1;
            if dial_count == 1 {
                return Err(DialError::Refused(CliError::IpcUnavailable {
                    detail: "session 7 is running but did not answer: it is restarting".to_string(),
                }));
            }
            Err(DialError::Refused(CliError::ProtocolVersionRefused {
                detail: "this session speaks protocol version 9".to_string(),
            }))
        },
        Instant::now() + RESTART_WINDOW_DURATION,
    );

    assert_eq!(
        redial_result.map(|joined| joined.client_id),
        Err(AttachmentEnding::RefusedAfterRestart(Box::new(
            CliError::ProtocolVersionRefused {
                detail: "this session speaks protocol version 9".to_string(),
            }
        )))
    );
    assert_eq!(dial_count, 2);
}

#[test]
fn a_restarted_remote_session_still_refusing_past_the_deadline_ends_with_that_refusal() {
    let mut dial_count = 0_u32;
    let redial_result = redial_restarted_remote_session_with(
        || {
            dial_count += 1;
            Err(DialError::Refused(CliError::IpcUnavailable {
                detail: "session 7 is running but did not answer: it is restarting".to_string(),
            }))
        },
        Instant::now(),
    );

    assert_eq!(
        redial_result.map(|joined| joined.client_id),
        Err(AttachmentEnding::RefusedAfterRestart(Box::new(
            CliError::IpcUnavailable {
                detail: "session 7 is running but did not answer: it is restarting".to_string(),
            }
        )))
    );
    assert_eq!(dial_count, 1);
}

#[test]
fn a_restarted_remote_session_refused_first_and_unreachable_past_the_deadline_ends_as_restarting() {
    let restart_deadline = Instant::now() + Duration::from_secs(2);
    let mut dial_count = 0_u32;
    let redial_result = redial_restarted_remote_session_with(
        || {
            dial_count += 1;
            if dial_count == 1 {
                return Err(DialError::Refused(CliError::IpcUnavailable {
                    detail: "session 7 is running but did not answer: it is restarting".to_string(),
                }));
            }
            thread::sleep(restart_deadline.saturating_duration_since(Instant::now()));
            Err(DialError::Unreachable(CliError::IpcUnavailable {
                detail: "connection refused".to_string(),
            }))
        },
        restart_deadline,
    );

    assert_eq!(
        redial_result.map(|joined| joined.client_id),
        Err(AttachmentEnding::Restarting)
    );
    assert_eq!(dial_count, 2);
}

#[test]
fn a_restarted_remote_session_still_unreachable_past_the_deadline_ends_the_redial() {
    let mut dial_count = 0_u32;
    let redial_result = redial_restarted_remote_session_with(
        || {
            dial_count += 1;
            Err(DialError::Unreachable(CliError::IpcUnavailable {
                detail: "connection refused".to_string(),
            }))
        },
        Instant::now(),
    );

    assert_eq!(
        redial_result.map(|joined| joined.client_id),
        Err(AttachmentEnding::Restarting)
    );
    assert_eq!(dial_count, 1);
}

#[test]
fn a_detach_and_a_session_end_both_succeed_and_name_no_session_to_join_next() {
    let session_id = SessionId::new();
    assert_eq!(
        report_attachment_ending(
            &build_local_home(),
            AttachmentEnding::Detached,
            session_id,
            None
        )
        .expect("a detach is a success"),
        None
    );
    assert_eq!(
        report_attachment_ending(
            &build_local_home(),
            AttachmentEnding::SessionEnded,
            session_id,
            None,
        )
        .expect("a session ending is a success"),
        None
    );
}

#[test]
fn a_switch_names_the_session_and_user_to_join_next() {
    let target_session_id = SessionId::new();
    assert_eq!(
        report_attachment_ending(
            &build_local_home(),
            AttachmentEnding::SwitchSession {
                session_id: target_session_id,
                owner_user_id: Some(1001),
            },
            SessionId::new(),
            None,
        )
        .expect("a switch is a success"),
        Some((target_session_id, Some(1001)))
    );
}

#[test]
fn the_restarting_frame_ends_the_stream_to_come_back_on_the_new_socket() {
    assert_eq!(
        classify_session_event(&Ok(SessionEvent::Restarting)),
        Some(AttachmentEnding::Restarting)
    );
}

/// The attach settings of a terminal that shows no images, at the default
/// frame rate.
const TEXT_ONLY_ATTACH_SETTINGS: AttachSettings = AttachSettings {
    graphics_support: terminal::GraphicsSupport::Unsupported,
    max_frames_per_second: DEFAULT_MAX_FRAMES_PER_SECOND,
};

/// The token a test's client attached under, which the wait watches for a
/// change.
const OLD_CONNECTION_TOKEN: &str = "the token this client attached under";

/// The token the image replacing the session mints when it binds again.
const NEW_CONNECTION_TOKEN: &str = "the token the new image minted";

/// The resume token the attach before the restart minted, which the return
/// after the restart presents.
const PREVIOUS_RESUME_TOKEN: &str = "the resume token the last attach minted";

/// The resume token a stand-in restarted session mints for the attach it
/// answers.
const MINTED_RESUME_TOKEN: &str = "the resume token the rejoin minted";

/// The Attach result naming `client_id` and `session_id`, echoing `pane_area`
/// and minting `resume_token`: a session named `session` with no tabs.
fn build_attached_result(
    client_id: ClientId,
    session_id: SessionId,
    pane_area: Option<PaneArea>,
    resume_token: Option<ConnectionToken>,
) -> IpcResult {
    IpcResult::Attached {
        client_id,
        session_id,
        session_structure: AttachedSessionStructureSnapshot {
            session_id,
            session_name: String::from("session"),
            tabs: Vec::new(),
        },
        resume_token,
        pane_area,
    }
}

/// A stand-in session that has just come back from replacing its own process
/// image: it binds `session_id`'s socket, advertises it under [`NEW_CONNECTION_TOKEN`],
/// and serves one connection.
///
/// `protocol_version_refusal_message` is the sentence it refuses the Hello with as
/// [`IpcErrorCode::UnsupportedVersion`], and `None` accepts the Hello.
/// A `Prepare` carrying an Attach is answered `Prepared`, and the Attach is
/// recorded in the returned slot. `attach_answer` is how it answers the
/// `Commit` that runs the Attach: `Ok` is the client id it hands back, with the
/// resume token [`MINTED_RESUME_TOKEN`], and `Err` is the sentence it refuses
/// with. A caller holding that answer finds its Attach already in the returned
/// slot.
fn spawn_restarted_session(
    runtime_directory: &Path,
    session_id: SessionId,
    protocol_version_refusal_message: Option<&'static str>,
    attach_answer: Result<ClientId, &'static str>,
) -> Arc<Mutex<Option<IpcRequestKind>>> {
    let socket_address = compute_socket_address(runtime_directory, session_id);
    let listener = Listener::bind(&socket_address).expect("bind the stand-in session");
    write_session_endpoint_file(
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
            let ipc_result = match &request.request_kind {
                IpcRequestKind::Hello { .. } => match protocol_version_refusal_message {
                    Some(refusal_message) => IpcResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::UnsupportedVersion,
                        message: String::from(refusal_message),
                    }),
                    None => build_accepted_hello_result(),
                },
                IpcRequestKind::Prepare(prepared_kind)
                    if matches!(**prepared_kind, IpcRequestKind::Attach { .. }) =>
                {
                    *request_kind_slot
                        .lock()
                        .expect("the slot outlives every panic") = Some((**prepared_kind).clone());
                    IpcResult::Prepared
                }
                IpcRequestKind::Commit => match attach_answer {
                    Ok(client_id) => build_attached_result(
                        client_id,
                        session_id,
                        None,
                        Some(ConnectionToken::from_secret(MINTED_RESUME_TOKEN)),
                    ),
                    Err(message) => IpcResult::Error(IpcErrorPayload {
                        code: IpcErrorCode::BadToken,
                        message: String::from(message),
                    }),
                },
                _ => IpcResult::Error(IpcErrorPayload {
                    code: IpcErrorCode::UnsupportedKind,
                    message: String::from("the stand-in session serves only a join"),
                }),
            };
            let _ = connection.send(&IpcResponse {
                request_id: Some(request.request_id),
                answer_result: ipc_result,
            });
        }
    });
    recorded_request_kind
}

/// The token the image after the next one mints, once the new image replaces
/// itself again.
const THIRD_CONNECTION_TOKEN: &str = "the token the image after it minted";

#[test]
fn an_attach_commit_answered_with_another_reply_kind_leaves_the_attach_unanswered() {
    let attach_error = parse_attached_session(IpcResult::Restarting)
        .expect_err("a Restarting does not answer an attach");

    let CliError::CommittedRequestUnanswered { detail } = attach_error else {
        panic!("expected CommittedRequestUnanswered, got {attach_error:?}");
    };
    assert_eq!(
        detail,
        "the session answered with an unexpected Restarting reply"
    );
}

#[test]
fn an_attach_commit_refused_while_restarting_is_session_restarting() {
    let attach_error =
        parse_attached_session(build_restarting_refusal()).expect_err("a refusal does not join");

    let CliError::SessionRestarting { detail } = attach_error else {
        panic!("expected SessionRestarting, got {attach_error:?}");
    };
    assert_eq!(detail, "the session is restarting; ask again in a moment");
}

#[test]
fn coming_back_to_a_session_that_restarts_again_waits_for_its_next_image_and_joins_it() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let session_thread = spawn_session_replacing_its_image(
        runtime_directory.path(),
        session_id,
        NEW_CONNECTION_TOKEN,
        THIRD_CONNECTION_TOKEN,
        |_| build_restarting_refusal(),
        move |request_kind| {
            let IpcRequestKind::Attach { .. } = request_kind else {
                panic!("expected an Attach, got {request_kind:?}");
            };
            build_attached_result(client_id, session_id, None, None)
        },
    );

    let joined_session = rejoin_session(
        runtime_directory.path(),
        session_id,
        client_id,
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        None,
        Instant::now() + RESTART_WINDOW_DURATION,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
    )
    .expect("the next image hands the client record back");

    assert_eq!(joined_session.client_id, client_id);
    assert_eq!(joined_session.session_id, session_id);
    assert_eq!(
        joined_session.connection_token,
        ConnectionToken::from_secret(THIRD_CONNECTION_TOKEN)
    );
    drop(joined_session);
    session_thread.join().expect("the stand-in session exits");
}

#[test]
fn an_attach_answer_is_taken_whether_or_not_it_echoes_a_pane_area() {
    let client_id = ClientId::new();
    let session_id = SessionId::new();

    let echoed_attached_session = parse_attached_session(build_attached_result(
        client_id,
        session_id,
        Some(PaneArea::Reported(Size {
            column_count: 80,
            row_count: 22,
        })),
        None,
    ))
    .expect("the answer of a session that echoes the field is accepted");
    let silent_attached_session =
        parse_attached_session(build_attached_result(client_id, session_id, None, None))
            .expect("the answer of an older session is accepted too");

    assert_eq!(echoed_attached_session, (client_id, session_id, None));
    assert_eq!(silent_attached_session, (client_id, session_id, None));
}

/// A stream half over bytes held in memory: reads come from `read_bytes`,
/// writes land in `written_bytes`, and each read and each write records the
/// deadline and the idle timeout the half held when it started.
struct RecordedLinkHalf {
    /// The bytes the reads return, in order.
    read_bytes: io::Cursor<Vec<u8>>,
    /// Every byte written, in order.
    written_bytes: Arc<Mutex<Vec<u8>>>,
    /// The deadline the last [`Deadlined::set_deadline`] gave and the idle
    /// timeout the last [`Deadlined::set_idle_timeout`] gave.
    link_limits: LinkLimits,
    /// The limits the half held when each read or write started, in order.
    link_limits_at_steps: Arc<Mutex<Vec<LinkLimits>>>,
}

/// The deadline and the idle timeout a [`RecordedLinkHalf`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LinkLimits {
    /// When every read or write must be finished by, or `None` for no limit.
    deadline: Option<Instant>,
    /// The longest one read or write waits for a byte, or `None` for no limit.
    idle_timeout: Option<Duration>,
}

/// A [`RecordedLinkHalf`] whose reads return `read_bytes`, with no deadline,
/// no idle timeout, nothing written and no step recorded.
fn build_recorded_link_half(read_bytes: Vec<u8>) -> RecordedLinkHalf {
    RecordedLinkHalf {
        read_bytes: io::Cursor::new(read_bytes),
        written_bytes: Arc::new(Mutex::new(Vec::new())),
        link_limits: LinkLimits {
            deadline: None,
            idle_timeout: None,
        },
        link_limits_at_steps: Arc::new(Mutex::new(Vec::new())),
    }
}

impl io::Read for RecordedLinkHalf {
    fn read(&mut self, read_buffer: &mut [u8]) -> io::Result<usize> {
        self.link_limits_at_steps
            .lock()
            .expect("the record outlives every panic")
            .push(self.link_limits);
        io::Read::read(&mut self.read_bytes, read_buffer)
    }
}

impl io::Write for RecordedLinkHalf {
    fn write(&mut self, frame_bytes: &[u8]) -> io::Result<usize> {
        self.link_limits_at_steps
            .lock()
            .expect("the record outlives every panic")
            .push(self.link_limits);
        self.written_bytes
            .lock()
            .expect("the record outlives every panic")
            .extend_from_slice(frame_bytes);
        Ok(frame_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Deadlined for RecordedLinkHalf {
    fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.link_limits.deadline = deadline;
    }

    fn set_idle_timeout(&mut self, idle_timeout: Option<Duration>) {
        self.link_limits.idle_timeout = idle_timeout;
    }
}

/// The frames a serving machine sends to join a client to `session_id` as
/// `client_id`, as bytes: the session's answer to the Hello, its `Prepared`
/// answer to the `Prepare`, numbered 2, its answer to the `Commit`, numbered 3,
/// then the string `after the join`.
fn encode_remote_join_answers(client_id: ClientId, session_id: SessionId) -> Vec<u8> {
    let encoding_half = build_recorded_link_half(Vec::new());
    let encoded_bytes = Arc::clone(&encoding_half.written_bytes);
    let (_, mut frame_writer) = build_frame_halves(
        Box::new(build_recorded_link_half(Vec::new())),
        Box::new(encoding_half),
    );
    frame_writer
        .send(&IpcResponse {
            request_id: Some(1),
            answer_result: build_accepted_hello_result(),
        })
        .expect("the Hello answer is encoded");
    frame_writer
        .send(&IpcResponse {
            request_id: Some(2),
            answer_result: IpcResult::Prepared,
        })
        .expect("the Prepare answer is encoded");
    frame_writer
        .send(&IpcResponse {
            request_id: Some(3),
            answer_result: build_attached_result(client_id, session_id, None, None),
        })
        .expect("the Commit answer is encoded");
    frame_writer
        .send(&"after the join")
        .expect("the frame after the join is encoded");
    let remote_join_answer_bytes = encoded_bytes
        .lock()
        .expect("the record outlives every panic")
        .clone();
    remote_join_answer_bytes
}

/// Asserts that `link_limits_at_steps` holds at least one step, that every
/// step held `expected_link_limits`, and then clears the record.
#[track_caller]
fn assert_every_step_held_and_clear(
    link_limits_at_steps: &Mutex<Vec<LinkLimits>>,
    expected_link_limits: LinkLimits,
) {
    let mut link_limits_at_steps = link_limits_at_steps
        .lock()
        .expect("the record outlives every panic");
    assert!(!link_limits_at_steps.is_empty());
    assert_eq!(
        *link_limits_at_steps,
        vec![expected_link_limits; link_limits_at_steps.len()]
    );
    link_limits_at_steps.clear();
}

/// Asserts that `link_limits_at_steps` holds at least one step holding
/// `prepare_link_limits`, then at least one step, each holding one deadline
/// inside `commit_deadline_range` and no idle timeout, and nothing else. Then
/// clears the record.
#[track_caller]
fn assert_prepare_then_commit_steps_and_clear(
    link_limits_at_steps: &Mutex<Vec<LinkLimits>>,
    prepare_link_limits: LinkLimits,
    commit_deadline_range: std::ops::RangeInclusive<Instant>,
) {
    let mut link_limits_at_steps = link_limits_at_steps
        .lock()
        .expect("the record outlives every panic");
    let prepare_step_count = link_limits_at_steps
        .iter()
        .take_while(|link_limits| **link_limits == prepare_link_limits)
        .count();
    assert!(prepare_step_count > 0);
    let commit_link_limits = *link_limits_at_steps
        .get(prepare_step_count)
        .expect("a step runs after the Prepare is answered");
    assert_eq!(commit_link_limits.idle_timeout, None);
    let commit_deadline = commit_link_limits
        .deadline
        .expect("the Commit runs under a deadline");
    assert!(
        commit_deadline_range.contains(&commit_deadline),
        "the Commit deadline {commit_deadline:?} is outside {commit_deadline_range:?}"
    );
    let commit_step_count = link_limits_at_steps.len() - prepare_step_count;
    assert_eq!(
        link_limits_at_steps[prepare_step_count..],
        vec![commit_link_limits; commit_step_count]
    );
    link_limits_at_steps.clear();
}

#[test]
fn a_remote_join_prepares_under_the_link_deadline_commits_under_a_fresh_join_timeout_then_keeps_only_the_idle_timeout(
) {
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let link_reader_half =
        build_recorded_link_half(encode_remote_join_answers(client_id, session_id));
    let read_link_limits = Arc::clone(&link_reader_half.link_limits_at_steps);
    let link_writer_half = build_recorded_link_half(Vec::new());
    let write_link_limits = Arc::clone(&link_writer_half.link_limits_at_steps);
    let (mut frame_reader, mut frame_writer) =
        build_frame_halves(Box::new(link_reader_half), Box::new(link_writer_half));
    let link_deadline = Instant::now() + remote_client::JOIN_TIMEOUT_DURATION * 2;
    frame_reader.set_deadline(Some(link_deadline));
    frame_writer.set_deadline(Some(link_deadline));
    let connection_token = ConnectionToken::generate();
    let join_started_at = Instant::now();

    let joined_session = join_remote_session(
        remote_client::RemoteLink {
            frame_reader,
            frame_writer,
        },
        connection_token.clone(),
        &SessionSelector::SessionId(session_id),
        None,
        None,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
        MIN_LINK_TIMEOUT_MS,
    )
    .expect("the session joins");
    let join_ended_at = Instant::now();

    assert_eq!(joined_session.client_id, client_id);
    assert_eq!(joined_session.session_id, session_id);
    assert_eq!(joined_session.connection_token, connection_token);
    assert_eq!(joined_session.resume_token, None);
    let prepare_link_limits = LinkLimits {
        deadline: Some(link_deadline),
        idle_timeout: None,
    };
    let commit_deadline_range = (join_started_at + remote_client::JOIN_TIMEOUT_DURATION)
        ..=(join_ended_at + remote_client::JOIN_TIMEOUT_DURATION);
    assert_prepare_then_commit_steps_and_clear(
        &read_link_limits,
        prepare_link_limits,
        commit_deadline_range.clone(),
    );
    assert_prepare_then_commit_steps_and_clear(
        &write_link_limits,
        prepare_link_limits,
        commit_deadline_range,
    );
    let JoinedSession {
        mut reader,
        mut writer,
        ..
    } = joined_session;

    writer
        .send(&"from the client")
        .expect("the frame is written");
    let received_text: String = reader.recv().expect("the frame after the join is read");

    assert_eq!(received_text, "after the join");
    let joined_link_limits = LinkLimits {
        deadline: None,
        idle_timeout: Some(Duration::from_millis(u64::from(MIN_LINK_TIMEOUT_MS))),
    };
    assert_every_step_held_and_clear(&read_link_limits, joined_link_limits);
    assert_every_step_held_and_clear(&write_link_limits, joined_link_limits);
}

#[test]
fn a_remote_join_names_its_placement_target_and_pings_every_third_of_the_link_timeout_until_answered(
) {
    let (client_connection, serving_connection) = connect_test_socket_pair();
    let (client_frame_reader, client_frame_writer) = client_connection.split();
    let (mut serving_frame_reader, mut serving_frame_writer) = serving_connection.split();
    serving_frame_reader.set_deadline(Some(Instant::now() + Duration::from_secs(10)));
    let client_id = ClientId::new();
    let session_id = SessionId::new();
    let placement_preview_target = PanePlacementPreviewTarget {
        source_pane_id: PaneId::new(),
        destination_tab_id: TabId::new(),
    };
    let (join_result_sender, join_result_receiver) = mpsc::channel();
    thread::spawn(move || {
        let join_result = join_remote_session(
            remote_client::RemoteLink {
                frame_reader: client_frame_reader,
                frame_writer: client_frame_writer,
            },
            ConnectionToken::generate(),
            &SessionSelector::SessionId(session_id),
            None,
            None,
            TEXT_ONLY_ATTACH_SETTINGS,
            None,
            Some(placement_preview_target),
            MIN_LINK_TIMEOUT_MS,
        );
        let _ = join_result_sender.send(
            join_result.map(|joined_session| (joined_session.client_id, joined_session.session_id)),
        );
    });

    let remote_attach: RemoteClientFrame = serving_frame_reader
        .recv()
        .expect("the serving machine's Attach arrives");
    // The session's Attach goes up after the Hello answer arrives.
    let hello_answer_sent_at = Instant::now();
    serving_frame_writer
        .send(&IpcResponse {
            request_id: Some(1),
            answer_result: build_accepted_hello_result(),
        })
        .expect("the Hello answer is written");
    let session_prepare: IpcRequest = serving_frame_reader
        .recv()
        .expect("the session's Prepare arrives");
    // The Prepare's answer is held back until two pings have arrived.
    let pings_before_prepared: Vec<IpcRequest> = (0..2)
        .map(|_| {
            serving_frame_reader
                .recv()
                .expect("a ping arrives while the Prepare waits")
        })
        .collect();
    let second_ping_delay = hello_answer_sent_at.elapsed();
    let prepared_sent_at = Instant::now();
    serving_frame_writer
        .send(&IpcResponse {
            request_id: Some(2),
            answer_result: IpcResult::Prepared,
        })
        .expect("the Prepare answer is written");
    let session_commit = loop {
        let session_request: IpcRequest = serving_frame_reader
            .recv()
            .expect("the session's Commit arrives");
        if session_request.request_kind != IpcRequestKind::Ping {
            break session_request;
        }
    };
    // The Commit's answer is held back until two pings have arrived.
    let pings_before_attached: Vec<IpcRequest> = (0..2)
        .map(|_| {
            serving_frame_reader
                .recv()
                .expect("a ping arrives while the Commit waits")
        })
        .collect();
    let second_commit_ping_delay = prepared_sent_at.elapsed();
    serving_frame_writer
        .send(&IpcResponse {
            request_id: Some(3),
            answer_result: build_attached_result(client_id, session_id, None, None),
        })
        .expect("the Commit answer is written");
    let joined_ids = join_result_receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the join ends")
        .expect("the session joins");

    assert_eq!(
        remote_attach,
        RemoteClientFrame::Attach {
            session_selector: SessionSelector::SessionId(session_id),
            link_timeout_ms: Some(MIN_LINK_TIMEOUT_MS),
        }
    );
    assert_eq!(session_prepare.request_id, 2);
    let IpcRequestKind::Prepare(prepared_kind) = session_prepare.request_kind else {
        panic!("the second frame is the session's Prepare");
    };
    let IpcRequestKind::Attach {
        link_timeout_ms,
        placement_preview_target: attached_placement_preview_target,
        ..
    } = *prepared_kind
    else {
        panic!("the Prepare carries the session's Attach");
    };
    assert_eq!(
        (link_timeout_ms, attached_placement_preview_target),
        (Some(MIN_LINK_TIMEOUT_MS), Some(placement_preview_target))
    );
    let join_ping = IpcRequest {
        request_id: 2,
        request_kind: IpcRequestKind::Ping,
    };
    assert_eq!(
        pings_before_prepared,
        vec![join_ping.clone(), join_ping.clone()]
    );
    assert!(
        second_ping_delay >= compute_heartbeat_interval(MIN_LINK_TIMEOUT_MS) * 2,
        "the second ping came {second_ping_delay:?} after the Hello answer"
    );
    assert_eq!(
        session_commit,
        IpcRequest {
            request_id: 3,
            request_kind: IpcRequestKind::Commit,
        }
    );
    assert_eq!(pings_before_attached, vec![join_ping.clone(), join_ping]);
    assert!(
        second_commit_ping_delay >= compute_heartbeat_interval(MIN_LINK_TIMEOUT_MS) * 2,
        "the second ping after the Commit came {second_commit_ping_delay:?} after the Prepare answer"
    );
    assert_eq!(joined_ids, (client_id, session_id));
}

#[test]
fn coming_back_after_a_restart_presents_the_resume_token_and_keeps_the_one_it_minted() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let recorded_attach_request =
        spawn_restarted_session(runtime_directory.path(), session_id, None, Ok(client_id));
    let advertised_endpoint = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), session_id),
    )
    .expect("the stand-in session advertised its socket");

    let joined_session = rejoin_session(
        runtime_directory.path(),
        session_id,
        client_id,
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        Some(&ConnectionToken::from_secret(PREVIOUS_RESUME_TOKEN)),
        Instant::now() + RESTART_WINDOW_DURATION,
        AttachSettings {
            graphics_support: terminal::GraphicsSupport::Kitty,
            max_frames_per_second: DEFAULT_MAX_FRAMES_PER_SECOND,
        },
        None,
        None,
    )
    .expect("the stand-in session handed the client record back");
    assert_eq!(joined_session.client_id, client_id);
    assert_eq!(
        joined_session.connection_token,
        advertised_endpoint.connection_token
    );
    assert_eq!(
        joined_session.resume_token,
        Some(ConnectionToken::from_secret(MINTED_RESUME_TOKEN))
    );
    drop(joined_session);
    assert_eq!(
        recorded_attach_request
            .lock()
            .expect("the slot outlives every panic")
            .clone(),
        Some(IpcRequestKind::Attach {
            viewport_size: get_terminal_viewport_size(),
            resume_client_id: Some(client_id),
            resume_token: Some(ConnectionToken::from_secret(PREVIOUS_RESUME_TOKEN)),
            pane_area: Some(compute_core_pane_area(get_terminal_viewport_size())),
            graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities {
                supports_kitty: true,
                supports_iterm: false,
                supports_sixel: false,
            },
            cell_size: None,
            link_timeout_ms: None,
            max_frames_per_second: DEFAULT_MAX_FRAMES_PER_SECOND,
            placement_preview_target: None,
        }),
        "the restart path claims the client record with its token and reports this terminal's capability"
    );
}

/// The Attach that `prepare_request`, a `Prepare` built by
/// [`build_attach_prepare_request`], carries.
#[track_caller]
fn take_prepared_attach(prepare_request: IpcRequest) -> IpcRequestKind {
    assert_eq!(prepare_request.request_id, 2);
    let IpcRequestKind::Prepare(prepared_kind) = prepare_request.request_kind else {
        panic!("expected a Prepare");
    };
    *prepared_kind
}

#[test]
fn attach_asks_for_the_frame_rate_its_settings_name() {
    for max_frames_per_second in [30, 120] {
        let attach_request = build_attach_prepare_request(
            None,
            None,
            AttachSettings {
                graphics_support: terminal::GraphicsSupport::Unsupported,
                max_frames_per_second,
            },
            None,
            None,
            None,
        );
        let IpcRequestKind::Attach {
            max_frames_per_second: sent_max_frames_per_second,
            ..
        } = take_prepared_attach(attach_request)
        else {
            panic!("expected an attach request");
        };
        assert_eq!(sent_max_frames_per_second, max_frames_per_second);
    }
}

#[test]
fn attach_advertises_the_selected_native_protocol() {
    let iterm_attach_request = build_attach_prepare_request(
        None,
        None,
        AttachSettings {
            graphics_support: terminal::GraphicsSupport::Iterm,
            max_frames_per_second: DEFAULT_MAX_FRAMES_PER_SECOND,
        },
        None,
        None,
        None,
    );
    let sixel_attach_request = build_attach_prepare_request(
        None,
        None,
        AttachSettings {
            graphics_support: terminal::GraphicsSupport::Sixel {
                palette_color_count: 2,
                maximum_pixel_width: None,
                maximum_pixel_height: None,
            },
            max_frames_per_second: DEFAULT_MAX_FRAMES_PER_SECOND,
        },
        None,
        None,
        None,
    );

    let IpcRequestKind::Attach {
        graphics_capabilities,
        ..
    } = take_prepared_attach(iterm_attach_request)
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
    } = take_prepared_attach(sixel_attach_request)
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
fn a_restarted_session_that_refuses_the_join_ends_with_that_refusal() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let _recorded_attach_request = spawn_restarted_session(
        runtime_directory.path(),
        session_id,
        None,
        Err("this session holds no such client"),
    );

    let rejoin_result = rejoin_session(
        runtime_directory.path(),
        session_id,
        client_id,
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        None,
        Instant::now() + RESTART_WINDOW_DURATION,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
    );
    assert_eq!(
        rejoin_result.map(|joined_session| joined_session.client_id),
        Err(AttachmentEnding::RefusedAfterRestart(Box::new(
            CliError::IpcUnavailable {
                detail: "this session holds no such client".to_string(),
            }
        )))
    );
}

#[test]
fn a_restarted_session_that_closes_before_answering_the_join_ends_as_restarting() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let listener = Listener::bind(&compute_socket_address(
        runtime_directory.path(),
        session_id,
    ))
    .expect("bind the stand-in session");
    write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        NEW_CONNECTION_TOKEN,
        std::process::id(),
    );
    let stand_in_session_thread = thread::spawn(move || {
        let mut connection = listener.accept().expect("the client connects");
        let _hello_request: IpcRequest = connection.recv().expect("the client writes its Hello");
    });

    let rejoin_result = rejoin_session(
        runtime_directory.path(),
        session_id,
        ClientId::new(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        None,
        Instant::now() + RESTART_WINDOW_DURATION,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
    );
    stand_in_session_thread
        .join()
        .expect("the stand-in session read the Hello and closed");

    assert_eq!(
        rejoin_result.map(|joined_session| joined_session.client_id),
        Err(AttachmentEnding::Restarting)
    );
}

#[test]
fn a_restarted_session_whose_new_socket_takes_no_connection_ends_as_restarting() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        NEW_CONNECTION_TOKEN,
        std::process::id(),
    );

    let rejoin_result = rejoin_session(
        runtime_directory.path(),
        session_id,
        ClientId::new(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        None,
        Instant::now() + RESTART_WINDOW_DURATION,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
    );

    assert_eq!(
        rejoin_result.map(|joined_session| joined_session.client_id),
        Err(AttachmentEnding::Restarting)
    );
}

#[test]
fn a_restarted_session_that_never_answers_the_join_ends_as_restarting_at_the_deadline() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let listener = Listener::bind(&compute_socket_address(
        runtime_directory.path(),
        session_id,
    ))
    .expect("bind the stand-in session");
    write_session_endpoint_file(
        runtime_directory.path(),
        session_id,
        NEW_CONNECTION_TOKEN,
        std::process::id(),
    );
    let stand_in_session_thread = thread::spawn(move || {
        let mut connection = listener.accept().expect("the client connects");
        let mut unanswered_request_count = 0_usize;
        while connection.recv::<IpcRequest>().is_ok() {
            unanswered_request_count += 1;
        }
        unanswered_request_count
    });
    let restart_deadline = Instant::now() + Duration::from_millis(300);

    let rejoin_result = rejoin_session(
        runtime_directory.path(),
        session_id,
        ClientId::new(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        None,
        restart_deadline,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
    );
    let rejoin_returned_at = Instant::now();

    assert_eq!(
        rejoin_result.map(|joined_session| joined_session.client_id),
        Err(AttachmentEnding::Restarting)
    );
    assert!(rejoin_returned_at < restart_deadline + Duration::from_secs(2));
    assert_eq!(
        stand_in_session_thread
            .join()
            .expect("the stand-in session read until the client closed"),
        2,
        "the client wrote its Hello and its Attach, then closed at the deadline"
    );
}

#[test]
fn a_viewer_whose_restarted_session_refuses_the_join_ends_with_that_refusal() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let _recorded_attach_request = spawn_restarted_session(
        runtime_directory.path(),
        session_id,
        None,
        Err("this session holds no such client"),
    );

    let (attachment_ending, _client) = run_local_attachment_through_restart(
        runtime_directory.path(),
        session_id,
        ClientId::new(),
        build_test_client(),
    );

    assert_eq!(
        attachment_ending,
        AttachmentEnding::RefusedAfterRestart(Box::new(CliError::IpcUnavailable {
            detail: "this session holds no such client".to_string(),
        }))
    );
}

#[test]
fn a_restarted_session_that_mints_a_new_client_is_joined_as_that_client() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let minted_client_id = ClientId::new();
    let _recorded_attach_request = spawn_restarted_session(
        runtime_directory.path(),
        session_id,
        None,
        Ok(minted_client_id),
    );
    let advertised_endpoint = EndpointFile::load_from_path(
        &EndpointFile::resolve_endpoint_file_path(runtime_directory.path(), session_id),
    )
    .expect("the stand-in session advertised its socket");

    let joined_session = rejoin_session(
        runtime_directory.path(),
        session_id,
        ClientId::new(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        None,
        Instant::now() + RESTART_WINDOW_DURATION,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
    )
    .expect("the restarted session is joined as the client it minted");
    assert_eq!(joined_session.client_id, minted_client_id);
    assert_eq!(
        joined_session.connection_token,
        advertised_endpoint.connection_token
    );
    drop(joined_session);
}

#[test]
fn a_viewer_whose_restarted_session_mints_a_new_client_goes_on_as_that_client() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let minted_client_id = ClientId::new();
    let recorded_attach_request = spawn_restarted_session(
        runtime_directory.path(),
        session_id,
        None,
        Ok(minted_client_id),
    );

    // The stand-in session answers the viewer's resize with a response frame,
    // which the event stream cannot read, so the new connection ends there.
    let (attachment_ending, client) = run_local_attachment_through_restart(
        runtime_directory.path(),
        session_id,
        client_id,
        build_test_client(),
    );

    assert_eq!(attachment_ending, AttachmentEnding::ConnectionDied);
    assert_eq!(client.get_client_id(), minted_client_id);
    let IpcRequestKind::Attach {
        resume_client_id,
        resume_token,
        ..
    } = recorded_attach_request
        .lock()
        .expect("the slot outlives every panic")
        .clone()
        .expect("the viewer joined the restarted session")
    else {
        panic!("the recorded request is an attach");
    };
    assert_eq!(resume_client_id, Some(client_id));
    assert_eq!(
        resume_token,
        Some(ConnectionToken::from_secret(PREVIOUS_RESUME_TOKEN))
    );
}

#[test]
fn a_viewer_in_pane_placement_mode_names_its_placement_target_in_the_attach_after_a_restart() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let recorded_attach_request =
        spawn_restarted_session(runtime_directory.path(), session_id, None, Ok(client_id));
    let placement_preview_target = PanePlacementPreviewTarget {
        source_pane_id: PaneId::new(),
        destination_tab_id: TabId::new(),
    };
    let mut client = build_test_client();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: placement_preview_target.source_pane_id,
        source_tab_id: Some(TabId::new()),
        destination_tab_id: placement_preview_target.destination_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });

    // The stand-in session answers the viewer's resize with a response frame,
    // which the event stream cannot read, so the new connection ends there.
    let (attachment_ending, _client) = run_local_attachment_through_restart(
        runtime_directory.path(),
        session_id,
        client_id,
        client,
    );

    assert_eq!(attachment_ending, AttachmentEnding::ConnectionDied);
    let IpcRequestKind::Attach {
        placement_preview_target: attached_placement_preview_target,
        ..
    } = recorded_attach_request
        .lock()
        .expect("the slot outlives every panic")
        .clone()
        .expect("the viewer joined the restarted session")
    else {
        panic!("the recorded request is an attach");
    };
    assert_eq!(
        attached_placement_preview_target,
        Some(placement_preview_target)
    );
}

#[test]
fn another_local_users_restarting_session_is_not_waited_for() {
    let runtime_directory = build_test_runtime_directory();
    let redial_started_at = Instant::now();

    // The empty token is what a session another local user started is reached
    // with: this user reads no endpoint file for it, so there is none to watch.
    let rejoin_result = rejoin_session(
        runtime_directory.path(),
        SessionId::new(),
        ClientId::new(),
        &ConnectionToken::from_secret(""),
        None,
        Instant::now() + RESTART_WINDOW_DURATION,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
    );

    assert_eq!(
        rejoin_result.map(|joined_session| joined_session.client_id),
        Err(AttachmentEnding::Restarting)
    );
    assert!(
        redial_started_at.elapsed() < Duration::from_secs(1),
        "no wait was spent on a session this client cannot watch"
    );
}

#[test]
fn a_restart_this_client_cannot_come_back_from_reports_the_restart_and_the_attach_command() {
    let session_id = SessionId::new();
    let ending_error = report_attachment_ending(
        &build_local_home(),
        AttachmentEnding::Restarting,
        session_id,
        None,
    )
    .expect_err("a restart with no way back is an error");
    assert_eq!(
        ending_error.to_string(),
        format!(
            "the session restarted and this client could not reconnect to it\n  \
             run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&ending_error), CliExitCode::RuntimeAction);
}

#[test]
fn a_restart_whose_new_image_refuses_this_client_reports_the_refusal_and_the_attach_command() {
    let session_id = SessionId::new();
    let ending_error = report_attachment_ending(
        &build_local_home(),
        AttachmentEnding::RefusedAfterRestart(Box::new(CliError::IpcUnavailable {
            detail: "this session holds no such client".to_string(),
        })),
        session_id,
        None,
    )
    .expect_err("a refused restart is an error");
    assert_eq!(
        ending_error.to_string(),
        format!(
            "the session restarted and refused this client: IPC unavailable: this session holds \
             no such client\n  run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&ending_error), CliExitCode::RuntimeAction);
}

/// The sentence a session that speaks only a newer protocol refuses this
/// client's Hello with.
const PROTOCOL_VERSION_REFUSAL_MESSAGE: &str =
    "the client speaks an older protocol than this session";

#[test]
fn a_dial_refused_for_the_old_token_joins_once_the_session_advertises_its_own() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let session_thread = spawn_session_listening_before_it_advertises(
        runtime_directory.path(),
        session_id,
        OLD_CONNECTION_TOKEN,
        NEW_CONNECTION_TOKEN,
        move |request_kind| {
            assert_eq!(request_kind.get_request_kind_name(), "Attach");
            build_attached_result(client_id, session_id, None, None)
        },
    );

    let joined_session = dial_session(
        &Home::Local {
            runtime_directory: runtime_directory.path().to_path_buf(),
            owner_user_id: None,
        },
        &SessionSelector::SessionId(session_id),
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
        30_000,
    )
    .expect("the second dial presents the session's own token");

    assert_eq!(joined_session.client_id, client_id);
    assert_eq!(joined_session.session_id, session_id);
    assert_eq!(
        joined_session.connection_token,
        ConnectionToken::from_secret(NEW_CONNECTION_TOKEN)
    );
    drop(joined_session);
    session_thread.join().expect("the stand-in session exits");
}

#[test]
fn a_restarted_session_that_refuses_this_protocol_version_is_a_restart_into_an_incompatible_build()
{
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let _recorded_attach_request = spawn_restarted_session(
        runtime_directory.path(),
        session_id,
        Some(PROTOCOL_VERSION_REFUSAL_MESSAGE),
        Ok(ClientId::new()),
    );

    let rejoin_result = rejoin_session(
        runtime_directory.path(),
        session_id,
        ClientId::new(),
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        None,
        Instant::now() + RESTART_WINDOW_DURATION,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
    );
    assert_eq!(
        rejoin_result.map(|joined_session| joined_session.client_id),
        Err(AttachmentEnding::RestartedIntoIncompatibleBuild)
    );
}

#[test]
fn a_viewer_whose_restarted_session_refuses_its_protocol_version_ends_as_a_restart_into_an_incompatible_build(
) {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let _recorded_attach_request = spawn_restarted_session(
        runtime_directory.path(),
        session_id,
        Some(PROTOCOL_VERSION_REFUSAL_MESSAGE),
        Ok(client_id),
    );

    let (attachment_ending, _client) = run_local_attachment_through_restart(
        runtime_directory.path(),
        session_id,
        client_id,
        build_test_client(),
    );

    assert_eq!(
        attachment_ending,
        AttachmentEnding::RestartedIntoIncompatibleBuild
    );
}

/// Run one attachment of `session_id` on this machine as `client_id` whose first
/// frame is `Restarting`, and hand back how it ended and the viewer it ran.
///
/// The viewer comes back on the socket a [`spawn_restarted_session`] stand-in
/// advertises in `runtime_directory`.
fn run_local_attachment_through_restart(
    runtime_directory: &Path,
    session_id: SessionId,
    client_id: ClientId,
    mut client: Client,
) -> (AttachmentEnding, Client) {
    client.set_client_id(client_id);
    let mut screen = build_test_screen();
    let (request_sender, _request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    incoming_sender
        .send(build_session_event_incoming(SessionEvent::Restarting))
        .expect("the loop's queue takes the restart frame");
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);

    let attachment_ending = run_attachment(
        &Home::Local {
            runtime_directory: runtime_directory.to_path_buf(),
            owner_user_id: None,
        },
        session_id,
        client_id,
        ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        Some(ConnectionToken::from_secret(PREVIOUS_RESUME_TOKEN)),
        &mut client,
        &mut screen,
        &mut uplink,
        TEXT_ONLY_ATTACH_SETTINGS,
        &mut cell_size_query,
        &mut Vec::new(),
        incoming_sender,
        incoming_receiver,
    );
    (attachment_ending, client)
}

#[test]
fn a_restart_into_an_incompatible_build_returns_only_after_the_router_restarts() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    write_router_endpoint_file(runtime_directory.path(), OLD_CONNECTION_TOKEN);
    let _recorded_attach_request = spawn_restarted_session(
        runtime_directory.path(),
        session_id,
        Some(PROTOCOL_VERSION_REFUSAL_MESSAGE),
        Ok(client_id),
    );
    let router_runtime_directory = runtime_directory.path().to_path_buf();
    let router_restart = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        let router_restarted_at = Instant::now();
        write_router_endpoint_file(&router_runtime_directory, NEW_CONNECTION_TOKEN);
        router_restarted_at
    });

    let reconnect_result = reconnect_after_restart(
        &Home::Local {
            runtime_directory: runtime_directory.path().to_path_buf(),
            owner_user_id: None,
        },
        session_id,
        client_id,
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        None,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
        30_000,
    );
    let reconnect_returned_at = Instant::now();

    assert_eq!(
        reconnect_result.map(|joined_session| joined_session.client_id),
        Err(AttachmentEnding::RestartedIntoIncompatibleBuild)
    );
    let router_restarted_at = router_restart.join().expect("the router restart ran");
    assert!(
        reconnect_returned_at >= router_restarted_at,
        "the reconnect returned before the router advertised its new token"
    );
}

#[test]
fn a_restart_into_an_incompatible_build_with_no_router_running_returns_at_once() {
    let runtime_directory = build_test_runtime_directory();
    let session_id = SessionId::new();
    let client_id = ClientId::new();
    let _recorded_attach_request = spawn_restarted_session(
        runtime_directory.path(),
        session_id,
        Some(PROTOCOL_VERSION_REFUSAL_MESSAGE),
        Ok(client_id),
    );
    let reconnect_started_at = Instant::now();

    let reconnect_result = reconnect_after_restart(
        &Home::Local {
            runtime_directory: runtime_directory.path().to_path_buf(),
            owner_user_id: None,
        },
        session_id,
        client_id,
        &ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        None,
        TEXT_ONLY_ATTACH_SETTINGS,
        None,
        None,
        30_000,
    );

    assert_eq!(
        reconnect_result.map(|joined_session| joined_session.client_id),
        Err(AttachmentEnding::RestartedIntoIncompatibleBuild)
    );
    assert!(
        reconnect_started_at.elapsed() < Duration::from_secs(1),
        "no router wait runs when no router was advertised"
    );
}

#[test]
fn a_restart_into_an_incompatible_build_with_no_client_path_reports_the_attach_command() {
    let session_id = SessionId::new();
    let ending_error = report_attachment_ending(
        &build_local_home(),
        AttachmentEnding::RestartedIntoIncompatibleBuild,
        session_id,
        None,
    )
    .expect_err("with no path to run, the restart is an error");
    assert_eq!(
        ending_error.to_string(),
        format!(
            "the session restarted into another koshi build and this client could not \
             reconnect to it: this client could not read the path it was started from\n  \
             run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`"
        )
    );
    assert_eq!(CliExitCode::from(&ending_error), CliExitCode::RuntimeAction);
}

#[test]
fn a_restart_into_an_incompatible_build_whose_client_path_cannot_start_reports_why_and_the_attach_command(
) {
    let session_id = SessionId::new();
    // A fresh directory holds no program at this path.
    let empty_directory = build_test_runtime_directory();
    let missing_executable_path = empty_directory.path().join("koshi");
    let expected_start_error = std::process::Command::new(&missing_executable_path)
        .status()
        .expect_err("no program is at that path");

    let ending_error = report_attachment_ending(
        &build_local_home(),
        AttachmentEnding::RestartedIntoIncompatibleBuild,
        session_id,
        Some(&missing_executable_path),
    )
    .expect_err("a command that cannot start is an error");
    assert_eq!(
        ending_error.to_string(),
        format!(
            "the session restarted into another koshi build and this client could not \
             reconnect to it: could not start {}: {expected_start_error}\n  \
             run `koshi list-sessions`; if session {session_id} is still listed, \
             reattach with `koshi attach {session_id}`",
            missing_executable_path.display()
        )
    );
    assert_eq!(CliExitCode::from(&ending_error), CliExitCode::RuntimeAction);
}

#[test]
fn the_attach_command_runs_the_client_executable_with_the_session_id_and_no_pane_marker() {
    let session_id = SessionId::new();
    let attach_command = build_attach_command(Path::new("/home/user/bin/koshi"), session_id);

    assert_eq!(
        attach_command.get_program(),
        std::ffi::OsStr::new("/home/user/bin/koshi")
    );
    assert_eq!(
        attach_command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<String>>(),
        vec![String::from("attach"), session_id.to_string()]
    );
    assert_eq!(
        attach_command.get_envs().collect::<Vec<_>>(),
        vec![(std::ffi::OsStr::new("KOSHI"), None)],
        "the command removes the pane marker and sets no variable"
    );
}

#[test]
fn a_target_with_a_slash_that_is_not_an_owned_session_id_is_refused() {
    let runtime_directory = build_test_runtime_directory();

    let target_result =
        resolve_local_session_target(runtime_directory.path(), None, "1001/quiet-lake");

    let Err(CliError::InvalidArgs { detail }) = target_result else {
        panic!("expected InvalidArgs, got {target_result:?}");
    };
    assert_eq!(
        detail,
        "expected `<user id>/session-<uuid>`, such as `1001/session-<uuid>`"
    );
}

#[cfg(unix)]
#[test]
fn an_owned_target_keeps_its_owner_once_that_users_folder_advertises_it() {
    use std::os::unix::fs::MetadataExt;

    // Every folder a test makes belongs to the user running it. With no
    // runtime directory, that user's id names another user.
    let shared_sessions_base_directory = build_test_runtime_directory();
    let runtime_directory = shared_sessions_base_directory
        .path()
        .join("missing-runtime");
    let folder_owner_user_id = std::fs::metadata(shared_sessions_base_directory.path())
        .expect("read the shared directory")
        .uid();
    let user_directory = shared_sessions_base_directory
        .path()
        .join((folder_owner_user_id + 1).to_string());
    std::fs::create_dir(&user_directory).expect("create a user's folder");
    let session_id = SessionId::new();
    drop(
        std::os::unix::net::UnixListener::bind(user_directory.join(format!("{session_id}.sock")))
            .expect("bind the session socket"),
    );
    let owned_target_text = format!("{folder_owner_user_id}/{session_id}");

    assert_eq!(
        resolve_local_session_target(
            &runtime_directory,
            Some(shared_sessions_base_directory.path()),
            &owned_target_text,
        )
        .expect("the folder advertises the session"),
        (session_id, Some(folder_owner_user_id))
    );
    let lookup_without_shared_directory =
        resolve_local_session_target(&runtime_directory, None, &owned_target_text);
    let Err(CliError::SessionNotFound { session_name }) = lookup_without_shared_directory else {
        panic!("expected SessionNotFound, got {lookup_without_shared_directory:?}");
    };
    assert_eq!(session_name, owned_target_text);
}

#[test]
fn the_way_back_to_another_users_session_names_its_owner() {
    let session_id = SessionId::new();
    let home = Home::Local {
        runtime_directory: PathBuf::from("/tmp/koshi-test"),
        owner_user_id: Some(1001),
    };

    assert_eq!(
        build_reattach_instructions(&home, session_id),
        format!(
            "run `koshi list-sessions`; if session {session_id} is still listed, reattach with \
             `koshi attach 1001/{session_id}`"
        )
    );
}

/// A frame of `mouse_panes`, laid out as full-width horizontal bands between the
/// tabline (row 0) and the hint bar (last row), with the first pane focused.
///
/// Two panes in an 80x24 viewport gives band rows 1..=11 and 12..=22, each with
/// a one-cell border ring, so the second band's top row is the divider the two
/// panes share.
fn build_mouse_frame(mouse_panes: &[MousePane]) -> MouseFrame {
    let tab_id = TabId::new();
    let band_row_count =
        (TEST_VIEWPORT_SIZE.row_count - 2) / u16::try_from(mouse_panes.len()).expect("few panes");
    let pane_slots: Vec<PaneSlot> = mouse_panes
        .iter()
        .enumerate()
        .map(|(pane_index, mouse_pane)| {
            let top_row = band_row_count * u16::try_from(pane_index).expect("few panes");
            PaneSlot {
                pane_id: mouse_pane.pane_id,
                outer_rect: Rect::from_origin_and_size(
                    Point {
                        column: 0,
                        row: top_row,
                    },
                    Size {
                        column_count: TEST_VIEWPORT_SIZE.column_count,
                        row_count: band_row_count,
                    },
                ),
                content_rect: Some(Rect::from_origin_and_size(
                    Point {
                        column: 1,
                        row: top_row + 1,
                    },
                    Size {
                        column_count: TEST_VIEWPORT_SIZE.column_count - 2,
                        row_count: band_row_count - 2,
                    },
                )),
                is_visible: true,
                is_suppressed: false,
            }
        })
        .collect();
    MouseFrame {
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
            session_name: String::from("fixture"),
            active_tab_snapshot: TabSnapshot {
                tab_id,
                tab_name: String::from("one"),
                pane_slots,
                tab_size: Size {
                    column_count: TEST_VIEWPORT_SIZE.column_count,
                    row_count: TEST_VIEWPORT_SIZE.row_count - 2,
                },
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMetadata {
                tab_id,
                tab_name: String::from("one"),
                tab_index: 0,
                is_active: true,
            }],
            floating_pane_slots: Vec::new(),
            floating_pane_ids: Vec::new(),
        },
        mouse_panes: mouse_panes.to_vec(),
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id: mouse_panes.first().map(|mouse_pane| mouse_pane.pane_id),
            active_tab_focused_pane_id: mouse_panes.first().map(|mouse_pane| mouse_pane.pane_id),
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            last_applied_placement_command_id: None,
            is_mouse_selection_enabled: false,
        },
        committed_regions: CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0),
        viewer_chrome: koshi_renderer::snapshot::ViewerChrome::default(),
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
        modifier_flags: BindingModifierFlags::NONE,
    }
}

/// A left press at `screen_point`.
fn build_mouse_press(screen_point: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Press(MouseButton::Left),
        position: screen_point,
        modifier_flags: BindingModifierFlags::NONE,
    }
}

/// A left drag to `screen_point`.
fn build_mouse_drag(screen_point: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Drag(MouseButton::Left),
        position: screen_point,
        modifier_flags: BindingModifierFlags::NONE,
    }
}

/// A left release at `screen_point`.
fn build_mouse_release(screen_point: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Release(MouseButton::Left),
        position: screen_point,
        modifier_flags: BindingModifierFlags::NONE,
    }
}

/// One highlight change for `pane_id`, ending at line `row_index`.
fn build_selection_action(pane_id: PaneId, row_index: u64) -> MouseAction {
    MouseAction::Command(Box::new(Command::Visual(VisualCommand::SetSelection(
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
    ))))
}

/// One press handed to `pane_id`'s program, at `column_index`.
fn build_forward_mouse_action(pane_id: PaneId, column_index: u16) -> MouseAction {
    MouseAction::Forward {
        pane_id,
        mouse_kind: MouseKind::Press(MouseButton::Left),
        modifier_flags: BindingModifierFlags::NONE,
        pane_local_cell: Point {
            column: column_index,
            row: 5,
        },
    }
}

/// A live control socket with the two ends a test needs: the client end wrapped
/// in the shape the loop writes through, and the session end the requests are
/// read back off.
///
/// The accept runs on its own thread while this thread connects.
struct TestControlSocket {
    /// The reading half of the client end. Held, not read: this is what the
    /// frame-reader thread owns in the running loop.
    _frame_reader: FrameReader,
    /// The session end: every request the uplink writes is read from here.
    session_connection: Connection,
    /// The client end, numbered from [`FIRST_POST_ATTACH_REQUEST_ID`] exactly as the
    /// loop numbers it.
    uplink: Uplink,
}

/// An uplink numbered from [`FIRST_POST_ATTACH_REQUEST_ID`] that queues every
/// request on `request_sender`, and whose writer thread reads as ended at once.
fn build_test_uplink(request_sender: mpsc::Sender<IpcRequest>) -> Uplink {
    Uplink {
        uplink_writer: Some(UplinkWriter {
            request_sender,
            writer_end_receiver: mpsc::channel().1,
        }),
        registry: ActionRegistry::new(),
        next_request_id: FIRST_POST_ATTACH_REQUEST_ID,
        sent_placement_preview_target: None,
    }
}

fn build_test_control_socket() -> TestControlSocket {
    let (client_connection, session_connection) = connect_test_socket_pair();
    let (frame_reader, frame_writer) = client_connection.split();
    TestControlSocket {
        _frame_reader: frame_reader,
        session_connection,
        uplink: Uplink {
            uplink_writer: Some(spawn_uplink_writer(frame_writer)),
            registry: ActionRegistry::new(),
            next_request_id: FIRST_POST_ATTACH_REQUEST_ID,
            sent_placement_preview_target: None,
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
        MouseAction::AlternateScrollArrows {
            pane_id,
            is_scrolling_up: false,
            arrow_count: 3,
        },
        MouseAction::AlternateScrollArrows {
            pane_id,
            is_scrolling_up: false,
            arrow_count: 5,
        },
    ];

    assert_eq!(
        coalesce_mouse_actions(alternate_scroll_actions),
        vec![MouseAction::AlternateScrollArrows {
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
fn two_highlight_changes_for_two_panes_stay_two_actions() {
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let selection_actions = vec![
        build_selection_action(first_pane_id, 12),
        build_selection_action(second_pane_id, 40),
    ];

    assert_eq!(
        coalesce_mouse_actions(selection_actions.clone()),
        selection_actions
    );
}

#[test]
fn a_highlight_change_after_a_clear_for_one_pane_stays_a_second_action() {
    let pane_id = PaneId::new();
    let selection_actions = vec![
        MouseAction::Command(Box::new(Command::Visual(VisualCommand::ClearSelection(
            ClearSelectionArgs { pane_id },
        )))),
        build_selection_action(pane_id, 40),
    ];

    assert_eq!(
        coalesce_mouse_actions(selection_actions.clone()),
        selection_actions
    );
}

#[test]
fn a_clear_after_a_highlight_change_for_one_pane_stays_a_second_action() {
    let pane_id = PaneId::new();
    let selection_actions = vec![
        build_selection_action(pane_id, 40),
        MouseAction::Command(Box::new(Command::Visual(VisualCommand::ClearSelection(
            ClearSelectionArgs { pane_id },
        )))),
    ];

    assert_eq!(
        coalesce_mouse_actions(selection_actions.clone()),
        selection_actions
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
fn a_corner_drags_border_moves_fold_to_the_newer_move_of_each_side() {
    let pane_id = PaneId::new();
    let build_border_move =
        |border_side: Direction, requested_cell_count: u16| MouseAction::Resize {
            pane_id,
            border_side,
            resize_step: 1,
            requested_cell_count,
        };

    assert_eq!(
        coalesce_mouse_actions(vec![
            build_border_move(Direction::Right, 3),
            build_border_move(Direction::Down, 2),
            build_border_move(Direction::Right, 5),
            build_border_move(Direction::Down, 4),
        ]),
        vec![
            build_border_move(Direction::Right, 5),
            build_border_move(Direction::Down, 4),
        ]
    );
}

#[test]
fn a_border_move_folds_past_no_move_on_its_own_axis_and_no_move_of_another_pane() {
    let pane_id = PaneId::new();
    let other_pane_id = PaneId::new();
    let build_border_move =
        |moved_pane_id: PaneId, border_side: Direction, requested_cell_count: u16| {
            MouseAction::Resize {
                pane_id: moved_pane_id,
                border_side,
                resize_step: 1,
                requested_cell_count,
            }
        };
    let same_axis_border_moves = vec![
        build_border_move(pane_id, Direction::Right, 3),
        build_border_move(pane_id, Direction::Left, 1),
        build_border_move(pane_id, Direction::Right, 5),
    ];
    let other_pane_border_moves = vec![
        build_border_move(pane_id, Direction::Right, 3),
        build_border_move(other_pane_id, Direction::Down, 1),
        build_border_move(pane_id, Direction::Right, 5),
    ];

    assert_eq!(
        coalesce_mouse_actions(same_axis_border_moves.clone()),
        same_axis_border_moves
    );
    assert_eq!(
        coalesce_mouse_actions(other_pane_border_moves.clone()),
        other_pane_border_moves
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
        "only neighbors fold, so the forward keeps the two scrolls apart"
    );
}

#[test]
fn an_empty_round_takes_no_request_id_and_writes_nothing() {
    let mut control_socket = build_test_control_socket();

    assert_eq!(
        send_mouse_round(&mut control_socket.uplink, Vec::new()),
        None
    );

    // The next request written is the first thing on the wire, and it carries
    // the id the empty round would have taken.
    let sentinel_request_id = control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    assert_eq!(sentinel_request_id, FIRST_POST_ATTACH_REQUEST_ID);
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the sentinel");
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
    let mut control_socket = build_test_control_socket();
    let mouse_round = vec![
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
        send_mouse_round(&mut control_socket.uplink, mouse_round),
        Some(FIRST_POST_ATTACH_REQUEST_ID)
    );

    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the round");
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
                    mouse_kind: MouseKind::Press(MouseButton::Left),
                    modifier_flags: BindingModifierFlags::NONE,
                    pane_local_cell: Point { column: 4, row: 5 },
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
    let sentinel_request_id = control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let sentinel_request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the sentinel");
    assert_eq!(
        sentinel_request,
        IpcRequest {
            request_id: sentinel_request_id,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

/// How many full rounds the writer-behind test queues while the session end
/// reads nothing.
///
/// Every round holds [`MAX_PENDING_MOUSE_ACTION_COUNT`] forwarded reports, about
/// 39 kB of frame. 128 rounds come to about 5.0 MB.
const WRITER_STALLED_ROUND_COUNT: u64 = 128;

/// How long the test waits for the queueing to finish before it fails.
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
            mouse_kind: MouseKind::Press(MouseButton::Left),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point {
                column: u16::try_from(column_index).expect("the cap fits a u16"),
                row: 5,
            },
        })
        .collect()
}

#[test]
fn queueing_a_round_returns_while_a_full_socket_holds_the_writer() {
    let pane_id = PaneId::new();
    let TestControlSocket {
        _frame_reader,
        mut session_connection,
        mut uplink,
    } = build_test_control_socket();
    let (queueing_done_sender, queueing_done_receiver) = mpsc::channel();

    // The session end reads nothing until every round is queued. The socket
    // fills, and the writer thread stops inside a write. The rounds are queued
    // on their own thread, which stands in for the loop.
    let queueing_thread = thread::spawn(move || {
        for round_index in 0..WRITER_STALLED_ROUND_COUNT {
            assert_eq!(
                send_mouse_round(&mut uplink, build_full_forwarded_mouse_round(pane_id)),
                Some(FIRST_POST_ATTACH_REQUEST_ID + round_index),
            );
        }
        queueing_done_sender
            .send(())
            .expect("the test is still waiting");
    });
    queueing_done_receiver
        .recv_timeout(QUEUEING_WATCHDOG_TIMEOUT_DURATION)
        .expect("every round was queued while the writer was stuck on the full socket");

    // Reading drains the socket and lets the writer run out. Every round
    // arrives whole and in the order it was queued: the backed-up queue drops
    // no forwarded report, folds none into another, and reorders none.
    for round_index in 0..WRITER_STALLED_ROUND_COUNT {
        let request: IpcRequest = session_connection.recv().expect("read the round");
        assert_eq!(
            request,
            IpcRequest {
                request_id: FIRST_POST_ATTACH_REQUEST_ID + round_index,
                request_kind: IpcRequestKind::Mouse(build_full_forwarded_wire_round(pane_id)),
            }
        );
    }
    queueing_thread
        .join()
        .expect("the queueing thread finished");
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

/// The session's answer to a move of the top border [`build_border_drag_fixture`]
/// grabbed on `pane_id`, `applied_cell_count` cells of it accepted.
fn build_top_border_resize_answer(pane_id: PaneId, applied_cell_count: u16) -> MouseAnswer {
    MouseAnswer::Resized {
        pane_id,
        border_side: Direction::Up,
        resize_step: -1,
        applied_cell_count,
    }
}

/// The record of a move of `pane_id`'s top border, `requested_cell_count` cells inward,
/// written in round `request_id` and not yet answered.
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
    let (mut client, mouse_frame, pointer_point) = build_border_drag_fixture();
    let mut sent_border_moves = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    apply_mouse_answers(
        &mut client,
        &mouse_frame,
        &mut sent_border_moves,
        8,
        vec![build_top_border_resize_answer(
            mouse_frame.mouse_panes[1].pane_id,
            3,
        )],
        &mut pending_mouse_actions,
    );

    assert_eq!(sent_border_moves, Vec::new());
    assert_eq!(pending_mouse_actions, Vec::new());
    assert_eq!(
        client.handle_mouse(
            build_mouse_drag(pointer_point),
            &mouse_frame,
            Instant::now()
        ),
        Vec::new(),
        "the answer names its own border, so the anchor moved to the pointer"
    );
}

#[test]
fn a_resized_forgets_only_the_border_move_its_own_round_wrote() {
    let (mut client, mouse_frame, _pointer_point) = build_border_drag_fixture();
    let pane_id = mouse_frame.mouse_panes[1].pane_id;
    let mut sent_border_moves = vec![
        build_sent_top_border_move(7, pane_id, 3),
        build_sent_top_border_move(8, pane_id, 1),
    ];
    let mut pending_mouse_actions = Vec::new();

    apply_mouse_answers(
        &mut client,
        &mouse_frame,
        &mut sent_border_moves,
        7,
        vec![build_top_border_resize_answer(pane_id, 3)],
        &mut pending_mouse_actions,
    );

    assert_eq!(
        sent_border_moves,
        vec![build_sent_top_border_move(8, pane_id, 1)],
        "round 8's move is still on the wire"
    );
    assert_eq!(
        compute_sent_border_cell_delta(&sent_border_moves, pane_id, Direction::Up),
        -1
    );
}

#[test]
fn an_empty_answer_changes_nothing() {
    let (mut client, mouse_frame, pointer_point) = build_border_drag_fixture();
    let mut sent_border_moves = vec![build_sent_top_border_move(
        7,
        mouse_frame.mouse_panes[1].pane_id,
        3,
    )];
    let mut pending_mouse_actions = Vec::new();

    apply_mouse_answers(
        &mut client,
        &mouse_frame,
        &mut sent_border_moves,
        7,
        Vec::new(),
        &mut pending_mouse_actions,
    );

    assert_eq!(
        sent_border_moves,
        vec![build_sent_top_border_move(
            7,
            mouse_frame.mouse_panes[1].pane_id,
            3
        )],
        "a round that reported no border move forgets none"
    );
    assert_eq!(pending_mouse_actions, Vec::new());
    assert_eq!(
        client.handle_mouse(
            build_mouse_drag(pointer_point),
            &mouse_frame,
            Instant::now()
        ),
        vec![MouseAction::Resize {
            pane_id: mouse_frame.mouse_panes[1].pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 3,
        }],
        "nothing was reported, so the anchor stayed where it was"
    );
}

#[test]
fn a_click_then_a_key_in_one_pass_reach_the_session_in_that_order() {
    let back_floating_pane_id = PaneId::new();
    let front_floating_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(PaneId::new())]);
    for (floating_pane_id, column) in [(back_floating_pane_id, 0), (front_floating_pane_id, 40)] {
        mouse_frame
            .session_snapshot
            .floating_pane_ids
            .push(floating_pane_id);
        mouse_frame
            .session_snapshot
            .floating_pane_slots
            .push(FloatingPaneSlot::from_outer_rect(
                floating_pane_id,
                Rect::from_origin_and_size(
                    Point { column, row: 2 },
                    Size {
                        column_count: 20,
                        row_count: 8,
                    },
                ),
                false,
            ));
        mouse_frame
            .mouse_panes
            .push(build_plain_mouse_pane(floating_pane_id));
    }
    mouse_frame.client_snapshot.focused_pane_id = Some(front_floating_pane_id);
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    client.pane_move_mode = Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
        selected_pane_id: front_floating_pane_id,
        draft_top_left_cell: None,
        pending_move_command_id: None,
    }));
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    let mut sent_border_moves = Vec::new();
    let mut pending_mouse_actions = Vec::new();
    // Screen `(5, 7)` is pane-area `(5, 6)`, inside the back pane's content.
    let back_pane_content_cell = Point { column: 5, row: 7 };

    // One pass: a click on the back pane, then Tab, then the pass's end.
    for runtime_event in [
        RuntimeEvent::MouseInput {
            client_id,
            mouse_input: build_mouse_press(back_pane_content_cell),
        },
        RuntimeEvent::MouseInput {
            client_id,
            mouse_input: build_mouse_release(back_pane_content_cell),
        },
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Named(NamedKey::Tab),
            )),
        },
    ] {
        assert_eq!(
            handle_terminal_input_event(
                &mut client,
                &mut uplink,
                &mut cell_size_query,
                Some(&mouse_frame),
                TerminalInput {
                    runtime_event,
                    read_at: Instant::now(),
                },
                &mut sent_border_moves,
                &mut pending_mouse_actions,
            ),
            ControlFlow::Continue(())
        );
    }
    flush_mouse_round(
        &mut uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );

    assert_eq!(
        request_receiver.try_recv().expect("the click's round"),
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Command(Box::new(
                Command::FocusPane(FocusPaneArgs {
                    focus_target: FocusTarget::Pane(back_floating_pane_id),
                    client_id: Some(client_id),
                })
            ))]),
        }
    );
    let tab_request = request_receiver.try_recv().expect("the Tab's command");
    assert_eq!(tab_request.request_id, FIRST_POST_ATTACH_REQUEST_ID + 1);
    let IpcRequestKind::SubmitCommand(tab_command_envelope) = tab_request.request_kind else {
        panic!("Tab submits a command, got {:?}", tab_request.request_kind);
    };
    assert_eq!(
        tab_command_envelope.command,
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(front_floating_pane_id),
            client_id: Some(client_id),
        })
    );
    assert_eq!(
        request_receiver.try_recv().err(),
        Some(mpsc::TryRecvError::Empty)
    );
    assert_eq!(
        client.pane_move_mode,
        Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
            selected_pane_id: front_floating_pane_id,
            draft_top_left_cell: None,
            pending_move_command_id: None,
        }))
    );
}

#[test]
fn held_mouse_actions_leave_before_the_requests_of_a_placement_handle_press() {
    let pane_id = PaneId::new();
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    client.placement_handle_pane_id = Some(pane_id);
    let painted_mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let active_tab_id = painted_mouse_frame.client_snapshot.active_tab_id;
    let mouse_frame = MouseFrame {
        viewer_chrome: client.build_viewer_chrome(active_tab_id),
        ..painted_mouse_frame
    };
    // The placement handle sits on the pane's top border, at screen `(1, 1)`.
    let handle_cell = Point { column: 1, row: 1 };
    assert_eq!(
        resolve_hit_region(mouse_frame.build_frame_layout(), handle_cell),
        HitRegion::PlacementHandle { pane_id }
    );
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    let mut sent_border_moves = Vec::new();
    // A wheel tick earlier in the same pass.
    let mut pending_mouse_actions = vec![MouseAction::Scroll {
        pane_id,
        is_scrolling_up: true,
        scroll_line_count: 3,
    }];

    assert_eq!(
        handle_terminal_input_event(
            &mut client,
            &mut uplink,
            &mut cell_size_query,
            Some(&mouse_frame),
            TerminalInput {
                runtime_event: RuntimeEvent::MouseInput {
                    client_id,
                    mouse_input: build_mouse_press(handle_cell),
                },
                read_at: Instant::now(),
            },
            &mut sent_border_moves,
            &mut pending_mouse_actions,
        ),
        ControlFlow::Continue(())
    );

    assert_eq!(pending_mouse_actions, Vec::new());
    let request_kinds: Vec<IpcRequestKind> = request_receiver
        .try_iter()
        .map(|ipc_request| ipc_request.request_kind)
        .collect();
    let [scroll_round, IpcRequestKind::SubmitCommand(focus_command_envelope)] =
        request_kinds.as_slice()
    else {
        panic!("expected a round and a command, got {request_kinds:?}");
    };
    assert_eq!(
        *scroll_round,
        IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
            pane_id,
            is_scrolling_up: true,
            scroll_line_count: 3,
        }])
    );
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id: pane_id,
            destination_tab_id: active_tab_id,
        })
    );
    assert_eq!(
        focus_command_envelope.command,
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(pane_id),
            client_id: Some(client_id),
        })
    );
}

#[test]
fn a_tick_reaches_the_wire_at_once_with_rounds_already_out() {
    let pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();
    let mut control_socket = build_test_control_socket();
    let mut sent_border_moves = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    // Four ticks, one per pass of the loop, none of them answered. Every pass
    // writes what it decided, so four rounds are on the wire unanswered.
    for round_index in 0..4u64 {
        handle_mouse_event(
            &mut client,
            &mouse_frame,
            build_mouse_wheel(ScrollDirection::Up, screen_point),
            Instant::now(),
            &mut pending_mouse_actions,
        );
        flush_mouse_round(
            &mut control_socket.uplink,
            &mut sent_border_moves,
            &mut pending_mouse_actions,
        );
        assert_eq!(
            pending_mouse_actions,
            Vec::new(),
            "the pass that decided it wrote it"
        );
        let request: IpcRequest = control_socket
            .session_connection
            .recv()
            .expect("read the round");
        assert_eq!(
            request,
            IpcRequest {
                request_id: FIRST_POST_ATTACH_REQUEST_ID + round_index,
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
        &mouse_frame,
        build_mouse_wheel(ScrollDirection::Up, screen_point),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    flush_mouse_round(
        &mut control_socket.uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );
    let sentinel_request_id = control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);

    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the fifth round");
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
    let sentinel_request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the sentinel");
    assert_eq!(
        sentinel_request,
        IpcRequest {
            request_id: sentinel_request_id,
            request_kind: IpcRequestKind::Discovery,
        }
    );
    assert_eq!(sent_border_moves, Vec::new(), "no border move was written");
}

#[test]
fn ten_ticks_in_one_pass_leave_as_one_scroll_of_the_summed_lines() {
    let pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let screen_point = get_content_cell(&mouse_frame, 0);
    let mut client = build_test_client();
    let mut control_socket = build_test_control_socket();
    let mut sent_border_moves = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    // A burst of ten ticks read out of the channel as one batch, so all ten are
    // decided before the pass writes. No clock is read anywhere here.
    for _ in 0..10 {
        handle_mouse_event(
            &mut client,
            &mouse_frame,
            build_mouse_wheel(ScrollDirection::Up, screen_point),
            Instant::now(),
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

    flush_mouse_round(
        &mut control_socket.uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );

    assert_eq!(pending_mouse_actions, Vec::new());
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the folded burst");
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
    let sentinel_request_id = control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let sentinel_request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the sentinel");
    assert_eq!(
        sentinel_request,
        IpcRequest {
            request_id: sentinel_request_id,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn a_run_of_drag_moves_in_one_pass_leaves_as_the_newest_highlight() {
    let pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let mut client = build_test_client();
    let mut control_socket = build_test_control_socket();
    let mut sent_border_moves = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    // The press arrives in a pass of its own and goes out as its own round, so
    // the drag moves below are the only thing the next pass holds.
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_press(get_content_cell(&mouse_frame, 0)),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    flush_mouse_round(
        &mut control_socket.uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the press");
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
            &mouse_frame,
            build_mouse_drag(screen_point),
            Instant::now(),
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
            |cursor| MouseAction::Command(Box::new(Command::Visual(VisualCommand::SetSelection(
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
            ))))
        )
        .to_vec(),
        "one whole highlight per move, none written yet"
    );

    flush_mouse_round(
        &mut control_socket.uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );

    assert_eq!(pending_mouse_actions, Vec::new());
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the folded run");
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
    let sentinel_request_id = control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let sentinel_request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the sentinel");
    assert_eq!(
        sentinel_request,
        IpcRequest {
            request_id: sentinel_request_id,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn every_report_in_one_pass_leaves_as_its_own_forward_in_order() {
    let plain_pane_id = PaneId::new();
    let watched_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[
        build_plain_mouse_pane(plain_pane_id),
        MousePane {
            mouse_tracking: MouseTracking::Normal,
            ..build_plain_mouse_pane(watched_pane_id)
        },
    ]);
    let watched_pane_point = get_content_cell(&mouse_frame, 1);
    let mut client = build_test_client();
    let mut control_socket = build_test_control_socket();
    let mut sent_border_moves = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    // Five ticks on the pane whose program reads the mouse, arriving together in
    // one pass. The first two are the same tick twice: the pair a fold would
    // join.
    let forwarded_mouse_inputs = [
        build_mouse_wheel(ScrollDirection::Up, watched_pane_point),
        build_mouse_wheel(ScrollDirection::Up, watched_pane_point),
        build_mouse_wheel(ScrollDirection::Down, watched_pane_point),
        build_mouse_wheel(
            ScrollDirection::Up,
            Point {
                column: watched_pane_point.column + 1,
                ..watched_pane_point
            },
        ),
        build_mouse_wheel(
            ScrollDirection::Up,
            Point {
                row: watched_pane_point.row + 1,
                ..watched_pane_point
            },
        ),
    ];
    for mouse_input in forwarded_mouse_inputs {
        handle_mouse_event(
            &mut client,
            &mouse_frame,
            mouse_input,
            Instant::now(),
            &mut pending_mouse_actions,
        );
    }

    // The watched pane's content starts at screen (1, 13), so the watched
    // point (2, 14) is the pane's cell (1, 1).
    let forwarded_pane_local_cells = [
        Point { column: 1, row: 1 },
        Point { column: 1, row: 1 },
        Point { column: 1, row: 1 },
        Point { column: 2, row: 1 },
        Point { column: 1, row: 2 },
    ];
    let forwarded_actions: Vec<MouseAction> = forwarded_mouse_inputs
        .iter()
        .zip(forwarded_pane_local_cells)
        .map(|(mouse_input, pane_local_cell)| MouseAction::Forward {
            pane_id: watched_pane_id,
            mouse_kind: mouse_input.mouse_kind,
            modifier_flags: mouse_input.modifier_flags,
            pane_local_cell,
        })
        .collect();
    assert_eq!(
        pending_mouse_actions, forwarded_actions,
        "one report per tick, none written yet"
    );

    flush_mouse_round(
        &mut control_socket.uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );

    assert_eq!(pending_mouse_actions, Vec::new());
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the round of reports");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(
                forwarded_mouse_inputs
                    .iter()
                    .zip(forwarded_pane_local_cells)
                    .map(|(mouse_input, pane_local_cell)| WireMouseAction::Forward {
                        pane_id: watched_pane_id,
                        mouse_kind: mouse_input.mouse_kind,
                        modifier_flags: mouse_input.modifier_flags,
                        pane_local_cell,
                    })
                    .collect(),
            ),
        },
        "five reports in, five reports out, in the order they happened"
    );

    // Nothing follows it: the five reports were one request.
    let sentinel_request_id = control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let sentinel_request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the sentinel");
    assert_eq!(
        sentinel_request,
        IpcRequest {
            request_id: sentinel_request_id,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn the_pile_stops_at_the_cap_and_keeps_every_forwarded_report() {
    let plain_pane_id = PaneId::new();
    let watched_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[
        build_plain_mouse_pane(plain_pane_id),
        MousePane {
            mouse_tracking: MouseTracking::Normal,
            ..build_plain_mouse_pane(watched_pane_id)
        },
    ]);
    let plain_pane_point = get_content_cell(&mouse_frame, 0);
    let watched_pane_point = get_content_cell(&mouse_frame, 1);
    let mut client = build_test_client();
    let mut pending_mouse_actions = Vec::new();
    let mut forwarded_actions = Vec::new();

    // One pass holding more ticks than the cap. Ticks in alternating directions
    // never fold, so each one adds an action. Every hundredth lands on the pane
    // whose program reads the mouse, at its own column, and goes up as a report
    // instead of a scroll.
    for tick_index in 0..MAX_PENDING_MOUSE_ACTION_COUNT + 300 {
        let scroll_direction = if tick_index % 2 == 0 {
            ScrollDirection::Up
        } else {
            ScrollDirection::Down
        };
        if tick_index % 100 == 0 {
            let screen_point = Point {
                column: watched_pane_point.column
                    + u16::try_from(tick_index / 100).expect("a handful of reports"),
                row: watched_pane_point.row,
            };
            let wheel_mouse_input = build_mouse_wheel(scroll_direction, screen_point);
            handle_mouse_event(
                &mut client,
                &mouse_frame,
                wheel_mouse_input,
                Instant::now(),
                &mut pending_mouse_actions,
            );
            forwarded_actions.push(MouseAction::Forward {
                pane_id: watched_pane_id,
                mouse_kind: wheel_mouse_input.mouse_kind,
                modifier_flags: BindingModifierFlags::NONE,
                pane_local_cell: Point {
                    column: 1 + u16::try_from(tick_index / 100).expect("a handful of reports"),
                    row: 1,
                },
            });
        } else {
            handle_mouse_event(
                &mut client,
                &mouse_frame,
                build_mouse_wheel(scroll_direction, plain_pane_point),
                Instant::now(),
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
    let (mut client, mouse_frame, pointer_point) = build_border_drag_fixture();
    let further_pointer_point = Point {
        row: pointer_point.row + 1,
        ..pointer_point
    };
    let mut control_socket = build_test_control_socket();
    let mut sent_border_moves = vec![build_sent_top_border_move(
        7,
        mouse_frame.mouse_panes[1].pane_id,
        3,
    )];
    let mut pending_mouse_actions = Vec::new();

    // One more cell of travel while round 7 is out. It is measured from the
    // drag anchor, which no answer has moved yet, so it names all four cells.
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(further_pointer_point),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    assert_eq!(
        pending_mouse_actions,
        vec![MouseAction::Resize {
            pane_id: mouse_frame.mouse_panes[1].pane_id,
            border_side: Direction::Up,
            resize_step: -1,
            requested_cell_count: 4,
        }]
    );

    apply_mouse_answers(
        &mut client,
        &mouse_frame,
        &mut sent_border_moves,
        7,
        vec![build_top_border_resize_answer(
            mouse_frame.mouse_panes[1].pane_id,
            3,
        )],
        &mut pending_mouse_actions,
    );
    assert_eq!(
        sent_border_moves,
        Vec::new(),
        "round 7 is answered and forgotten"
    );
    flush_mouse_round(
        &mut control_socket.uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );

    // Three of the four cells are already travelled, so the round that goes out
    // asks for the one the pointer is still ahead by.
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the next round");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Resize {
                pane_id: mouse_frame.mouse_panes[1].pane_id,
                border_side: Direction::Up,
                resize_step: -1,
                requested_cell_count: 1,
            }]),
        }
    );
}

#[test]
fn a_border_move_the_answer_covered_whole_leaves_nothing_to_send() {
    let (mut client, mouse_frame, pointer_point) = build_border_drag_fixture();
    let further_pointer_point = Point {
        row: pointer_point.row + 1,
        ..pointer_point
    };
    let mut control_socket = build_test_control_socket();
    let mut sent_border_moves = vec![build_sent_top_border_move(
        7,
        mouse_frame.mouse_panes[1].pane_id,
        3,
    )];
    let mut pending_mouse_actions = Vec::new();

    // The pointer goes one cell further and comes straight back, so the newest
    // buffered move names exactly the three cells round 7 asked for.
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(further_pointer_point),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(pointer_point),
        Instant::now(),
        &mut pending_mouse_actions,
    );

    apply_mouse_answers(
        &mut client,
        &mouse_frame,
        &mut sent_border_moves,
        7,
        vec![build_top_border_resize_answer(
            mouse_frame.mouse_panes[1].pane_id,
            3,
        )],
        &mut pending_mouse_actions,
    );
    flush_mouse_round(
        &mut control_socket.uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );

    assert_eq!(sent_border_moves, Vec::new(), "nothing was left to ask for");
    assert_eq!(pending_mouse_actions, Vec::new());
    // Nothing reached the wire: the sentinel is the first request read.
    let sentinel_request_id = control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the sentinel");
    assert_eq!(
        request,
        IpcRequest {
            request_id: sentinel_request_id,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn an_answer_that_lands_while_the_pointer_is_still_moves_the_drag_anchor() {
    let (mut client, mouse_frame, pointer_point) = build_border_drag_fixture();
    let pane_id = mouse_frame.mouse_panes[1].pane_id;
    let mut control_socket = build_test_control_socket();
    let mut sent_border_moves = vec![build_sent_top_border_move(7, pane_id, 3)];
    let mut pending_mouse_actions = Vec::new();

    // A fourth cell of travel while round 7 is out, then round 7's answer: the
    // anchor takes the three cells the session moved and the one cell left over
    // goes out as the next round.
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(Point {
            row: pointer_point.row + 1,
            ..pointer_point
        }),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    apply_mouse_answers(
        &mut client,
        &mouse_frame,
        &mut sent_border_moves,
        7,
        vec![build_top_border_resize_answer(
            mouse_frame.mouse_panes[1].pane_id,
            3,
        )],
        &mut pending_mouse_actions,
    );
    flush_mouse_round(
        &mut control_socket.uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );

    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the second round");
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
        &mouse_frame,
        &mut sent_border_moves,
        FIRST_POST_ATTACH_REQUEST_ID,
        vec![build_top_border_resize_answer(
            mouse_frame.mouse_panes[1].pane_id,
            1,
        )],
        &mut pending_mouse_actions,
    );
    assert_eq!(sent_border_moves, Vec::new());
    assert_eq!(pending_mouse_actions, Vec::new());

    // One more cell of travel past the border it moved: the anchor sits on the
    // border's real row, so one cell of pointer travel asks for one cell.
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(Point {
            row: pointer_point.row + 2,
            ..pointer_point
        }),
        Instant::now(),
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
    let (mut client, mouse_frame, pointer_point) = build_border_drag_fixture();
    let grabbed_pane_id = mouse_frame.mouse_panes[1].pane_id;
    let other_pane_id = mouse_frame.mouse_panes[0].pane_id;
    let further_pointer_point = Point {
        row: pointer_point.row + 1,
        ..pointer_point
    };
    let mut sent_border_moves = vec![
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
        &mouse_frame,
        build_mouse_drag(further_pointer_point),
        Instant::now(),
        &mut pending_mouse_actions,
    );

    apply_mouse_answers(
        &mut client,
        &mouse_frame,
        &mut sent_border_moves,
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

    assert_eq!(
        sent_border_moves,
        Vec::new(),
        "both of round 7's moves are answered"
    );
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
        client.handle_mouse(
            build_mouse_drag(further_pointer_point),
            &mouse_frame,
            Instant::now()
        ),
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
    let mut control_socket = build_test_control_socket();
    let mut sent_border_moves = Vec::new();
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

    flush_mouse_round(
        &mut control_socket.uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );

    assert_eq!(pending_mouse_actions, Vec::new());
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the round");
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
        sent_border_moves,
        vec![
            build_sent_top_border_move(FIRST_POST_ATTACH_REQUEST_ID, pane_id, 3),
            build_sent_top_border_move(FIRST_POST_ATTACH_REQUEST_ID, pane_id, 2),
        ],
        "five cells of border travel recorded for the round, not eight"
    );
}

#[test]
fn an_answer_for_a_border_drag_that_already_ended_changes_nothing() {
    let mouse_frame = build_mouse_frame(&[
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(PaneId::new()),
    ]);
    let first_pane_id = mouse_frame.mouse_panes[1].pane_id;
    let second_pane_id = mouse_frame.mouse_panes[2].pane_id;
    let upper_divider_point = Point {
        column: 10,
        row: mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[1]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let lower_divider_point = Point {
        column: 10,
        row: mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[2]
            .outer_rect
            .origin
            .row
            + 1,
    };
    let held_pointer_point = Point {
        row: lower_divider_point.row + 1,
        ..lower_divider_point
    };
    let mut client = build_test_client();
    let current_time = Instant::now();
    let mut sent_border_moves = vec![build_sent_top_border_move(7, first_pane_id, 3)];
    let mut pending_mouse_actions = Vec::new();

    // Round 7 asks for three cells of the upper divider.
    client.handle_mouse(
        build_mouse_press(upper_divider_point),
        &mouse_frame,
        current_time,
    );
    assert_eq!(
        client.handle_mouse(
            build_mouse_drag(Point {
                row: upper_divider_point.row + 3,
                ..upper_divider_point
            }),
            &mouse_frame,
            current_time
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
                row: upper_divider_point.row + 3,
                ..upper_divider_point
            }),
            &mouse_frame,
            current_time
        ),
        Vec::new()
    );
    client.handle_mouse(
        build_mouse_press(lower_divider_point),
        &mouse_frame,
        current_time,
    );
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(held_pointer_point),
        Instant::now(),
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
        &mouse_frame,
        &mut sent_border_moves,
        7,
        vec![build_top_border_resize_answer(first_pane_id, 3)],
        &mut pending_mouse_actions,
    );

    assert_eq!(
        sent_border_moves,
        Vec::new(),
        "round 7's move is answered and forgotten"
    );
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
        client.handle_mouse(
            build_mouse_drag(held_pointer_point),
            &mouse_frame,
            Instant::now()
        ),
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
    let mouse_frame = build_mouse_frame(&[
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(PaneId::new()),
    ]);
    let pane_id = mouse_frame.mouse_panes[1].pane_id;
    let grabbed_divider_point = get_border_divider_point(&mouse_frame);
    let mut client = build_test_client();
    let mut control_socket = build_test_control_socket();
    let mut sent_border_moves = Vec::new();
    let mut pending_mouse_actions = Vec::new();

    client.handle_mouse(
        build_mouse_press(grabbed_divider_point),
        &mouse_frame,
        Instant::now(),
    );

    // The pointer walks three cells down, one cell per pass, with none of the
    // rounds answered. Each move names its whole distance from the drag anchor —
    // 1, then 2, then 3 — and each round asks for the one cell the rounds
    // already on the wire did not.
    for travelled_cell_count in 1..=3u16 {
        handle_mouse_event(
            &mut client,
            &mouse_frame,
            build_mouse_drag(Point {
                row: grabbed_divider_point.row + travelled_cell_count,
                ..grabbed_divider_point
            }),
            Instant::now(),
            &mut pending_mouse_actions,
        );
        assert_eq!(
            pending_mouse_actions,
            vec![MouseAction::Resize {
                pane_id,
                border_side: Direction::Up,
                resize_step: -1,
                requested_cell_count: travelled_cell_count,
            }],
            "the anchor has not moved, so the whole distance is named again"
        );
        flush_mouse_round(
            &mut control_socket.uplink,
            &mut sent_border_moves,
            &mut pending_mouse_actions,
        );
        let request: IpcRequest = control_socket
            .session_connection
            .recv()
            .expect("read the round");
        assert_eq!(
            request,
            IpcRequest {
                request_id: FIRST_POST_ATTACH_REQUEST_ID + u64::from(travelled_cell_count) - 1,
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
        compute_sent_border_cell_delta(&sent_border_moves, pane_id, Direction::Up),
        -3,
        "three cells asked for over three rounds, one each"
    );

    // The three answers come back, each taking its own round's cell off.
    for round_index in 0..3u64 {
        apply_mouse_answers(
            &mut client,
            &mouse_frame,
            &mut sent_border_moves,
            FIRST_POST_ATTACH_REQUEST_ID + round_index,
            vec![build_top_border_resize_answer(pane_id, 1)],
            &mut pending_mouse_actions,
        );
    }

    assert_eq!(sent_border_moves, Vec::new());
    assert_eq!(
        client.handle_mouse(
            build_mouse_drag(Point {
                row: grabbed_divider_point.row + 3,
                ..grabbed_divider_point
            }),
            &mouse_frame,
            Instant::now()
        ),
        Vec::new(),
        "the anchor took all three cells, so the still pointer asks for nothing"
    );
}

#[test]
fn a_border_move_the_session_refused_stops_coming_off_the_next_one() {
    let (mut client, mouse_frame, pointer_point) = build_border_drag_fixture();
    let pane_id = mouse_frame.mouse_panes[1].pane_id;
    let mut control_socket = build_test_control_socket();
    let mut sent_border_moves = vec![build_sent_top_border_move(7, pane_id, 3)];
    let mut pending_mouse_actions = Vec::new();

    // Round 7 asked for three cells and the session took none: the border is
    // against a wall. The anchor stays put and the sent move is forgotten: the
    // pointer's next move asks for its whole distance again.
    apply_mouse_answers(
        &mut client,
        &mouse_frame,
        &mut sent_border_moves,
        7,
        vec![build_top_border_resize_answer(pane_id, 0)],
        &mut pending_mouse_actions,
    );
    assert_eq!(sent_border_moves, Vec::new());

    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(pointer_point),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    flush_mouse_round(
        &mut control_socket.uplink,
        &mut sent_border_moves,
        &mut pending_mouse_actions,
    );

    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the next round");
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
    let mut control_socket = build_test_control_socket();

    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Char('a'),
            )),
        },
        Instant::now(),
    );

    // The sentinel goes out behind the key. A key that was never sent reads
    // back as the sentinel.
    control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the key");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Keyboard {
                key_input: crate::tests::build_key_input_for_chord(KeyChord::from_parts(
                    BindingModifierFlags::NONE,
                    Key::Char('a'),
                )),
            },
        }
    );
}

/// The release the outer terminal reports for one chord.
fn build_key_release_for_chord(key_chord: KeyChord) -> KeyInput {
    KeyInput {
        key_event_kind: KeyEventKind::Release,
        ..crate::tests::build_key_input_for_chord(key_chord)
    }
}

#[test]
fn a_key_release_goes_up_the_connection_and_leaves_an_open_sequence_open() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut control_socket = build_test_control_socket();
    // `<C-p>` is the default pane prefix: it binds nothing on its own, opens a
    // sequence, and holds the keyboard.
    let sequence_opener_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p'));

    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(sequence_opener_chord),
        },
        Instant::now(),
    );
    let chords_after_opener = client
        .get_pending_key_sequence()
        .expect("the opener left a sequence open")
        .list_chords()
        .to_vec();

    let released_key_input = build_key_release_for_chord(sequence_opener_chord);
    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: released_key_input.clone(),
        },
        Instant::now(),
    );

    assert_eq!(
        client
            .get_pending_key_sequence()
            .expect("the sequence is still open")
            .list_chords(),
        chords_after_opener,
        "the release advanced no sequence"
    );

    // The opener was held: the release is the first thing this connection
    // carries. The sentinel goes out behind it.
    control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the release");
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
    let mut control_socket = build_test_control_socket();
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
        &mut control_socket.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: left_shift_key_input.clone(),
        },
        Instant::now(),
    );

    control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the key");
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
    let mut control_socket = build_test_control_socket();
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
        &mut control_socket.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: text_only_key_input.clone(),
        },
        Instant::now(),
    );

    control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the text-only event");
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
    // arriving after a fresh press leaves the next gesture running.
    let pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut control_socket = build_test_control_socket();
    let mut pending_mouse_actions = Vec::new();

    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_press(get_content_cell(&mouse_frame, 0)),
        Instant::now(),
        &mut pending_mouse_actions,
    );

    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: build_key_release_for_chord(KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Char('a'),
            )),
        },
        Instant::now(),
    );

    pending_mouse_actions.clear();
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(Point { column: 9, row: 8 }),
        Instant::now(),
        &mut pending_mouse_actions,
    );

    assert_eq!(
        pending_mouse_actions
            .last()
            .expect("the drag extended the gesture"),
        &MouseAction::Command(Box::new(Command::Visual(VisualCommand::SetSelection(
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
        ))))
    );
}

#[test]
fn a_key_the_pane_gets_ends_this_viewers_selection_gesture() {
    // The key goes to the program, and the highlight gesture over it ends. The
    // highlight it already made stands; only the drag ends.
    let pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut control_socket = build_test_control_socket();
    let mut pending_mouse_actions = Vec::new();

    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_press(get_content_cell(&mouse_frame, 0)),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(Point { column: 9, row: 8 }),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    assert_eq!(
        pending_mouse_actions
            .last()
            .expect("the drag decided something"),
        &MouseAction::Command(Box::new(Command::Visual(VisualCommand::SetSelection(
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
        ))))
    );

    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Char('a'),
            )),
        },
        Instant::now(),
    );

    // The same move again, with no gesture left to extend.
    pending_mouse_actions.clear();
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(Point {
            column: 20,
            row: 12,
        }),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    assert_eq!(pending_mouse_actions, Vec::new());
}

#[test]
fn a_terminal_resize_moves_the_viewers_own_size_and_tells_the_session() {
    // The resize moves the viewer's own size and reports the new size to the
    // session.
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut control_socket = build_test_control_socket();
    let resized_viewport_size = Size {
        column_count: 132,
        row_count: 43,
    };
    assert_eq!(client.get_viewport_size(), TEST_VIEWPORT_SIZE);
    let measured_cell_size = koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
        .expect("positive cell dimensions");

    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::Resize {
            client_id,
            viewport_size: resized_viewport_size,
            pane_area: None,
            cell_size: Some(measured_cell_size),
        },
        Instant::now(),
    );

    assert_eq!(
        client.get_viewport_size(),
        resized_viewport_size,
        "the viewer's own copy moved"
    );
    // The sentinel goes out behind the report. A report that was never sent
    // reads back as the sentinel.
    control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the resize");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Resize {
                viewport_size: resized_viewport_size,
                pane_area: Some(compute_core_pane_area(resized_viewport_size)),
                cell_size: None,
            },
        }
    );
}

#[test]
fn a_terminal_resize_reports_its_measured_cell_size() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut control_socket = build_test_control_socket();
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, true, false);
    let measured_cell_size = koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
        .expect("positive cell dimensions");

    assert_eq!(
        process_runtime_input_with_cell_size(
            &mut client,
            &mut control_socket.uplink,
            &mut cell_size_query,
            None,
            RuntimeEvent::Resize {
                client_id,
                viewport_size: Size {
                    column_count: 100,
                    row_count: 40,
                },
                pane_area: None,
                cell_size: Some(measured_cell_size),
            },
            Instant::now(),
        ),
        ControlFlow::Continue(())
    );

    control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the resize");
    let IpcRequestKind::Resize { cell_size, .. } = request.request_kind else {
        panic!("expected a Resize, got {:?}", request.request_kind);
    };
    assert_eq!(cell_size, Some(measured_cell_size));
}

#[test]
fn a_paste_goes_up_the_connection_as_the_text_the_terminal_delivered() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut control_socket = build_test_control_socket();

    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: String::from("hello\nworld"),
        },
        Instant::now(),
    );

    // The sentinel goes out behind the paste. A paste that was never sent
    // reads back as the sentinel.
    control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the paste");
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
    let mut control_socket = build_test_control_socket();

    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: "a".repeat(MAX_FRAME_BYTE_COUNT as usize + 1),
        },
        Instant::now(),
    );
    control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);

    // The read runs on its own thread, and a read that gets no frame within the
    // wait fails the test.
    let (read_result_sender, read_result_receiver) = mpsc::channel();
    let mut session_connection = control_socket.session_connection;
    thread::spawn(move || {
        let _ = read_result_sender.send(session_connection.recv::<IpcRequest>());
    });
    let request = read_result_receiver
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
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let mut control_socket = build_test_control_socket();
    let mut pending_mouse_actions = Vec::new();

    // Press and drag: the gesture is under way, and the move names a highlight.
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_press(get_content_cell(&mouse_frame, 0)),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(Point { column: 9, row: 8 }),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    assert_eq!(
        pending_mouse_actions
            .last()
            .expect("the drag decided something"),
        &MouseAction::Command(Box::new(Command::Visual(VisualCommand::SetSelection(
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
        ))))
    );

    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: String::from("hello"),
        },
        Instant::now(),
    );

    // The same move again, with no gesture left to extend.
    pending_mouse_actions.clear();
    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_drag(Point {
            column: 20,
            row: 12,
        }),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    assert_eq!(pending_mouse_actions, Vec::new());
}

#[test]
fn earliest_of_two_present_durations_is_the_smaller_either_order() {
    let shorter_duration = Duration::from_millis(5);
    let longer_duration = Duration::from_millis(50);
    assert_eq!(
        select_earliest_duration(Some(shorter_duration), Some(longer_duration)),
        Some(shorter_duration)
    );
    assert_eq!(
        select_earliest_duration(Some(longer_duration), Some(shorter_duration)),
        Some(shorter_duration)
    );
}

#[test]
fn earliest_of_two_equal_durations_returns_that_duration() {
    let equal_duration = Duration::from_millis(10);
    assert_eq!(
        select_earliest_duration(Some(equal_duration), Some(equal_duration)),
        Some(equal_duration)
    );
}

#[test]
fn earliest_falls_back_to_whichever_single_side_is_present() {
    let present_duration = Duration::from_millis(7);
    assert_eq!(
        select_earliest_duration(Some(present_duration), None),
        Some(present_duration)
    );
    assert_eq!(
        select_earliest_duration(None, Some(present_duration)),
        Some(present_duration)
    );
}

#[test]
fn earliest_of_two_absent_durations_is_none() {
    assert_eq!(select_earliest_duration(None, None), None);
}

/// A backend that can reject the buffer draw and the screen clear while
/// keeping the test terminal at a fixed size. It counts every draw and keeps
/// every cell an accepted draw wrote. An accepted clear of the whole screen
/// blanks the kept cells.
struct FailingBackend {
    terminal_size: RatatuiSize,
    should_fail_draw: bool,
    draw_count: Arc<AtomicUsize>,
    drawn_buffer: Arc<Mutex<Buffer>>,
}

impl FailingBackend {
    fn from_terminal_size(terminal_size: Size) -> Self {
        Self::from_terminal_size_with_draw_count(terminal_size, Arc::new(AtomicUsize::new(0)))
    }

    fn from_terminal_size_with_draw_count(
        terminal_size: Size,
        draw_count: Arc<AtomicUsize>,
    ) -> Self {
        FailingBackend {
            terminal_size: RatatuiSize {
                width: terminal_size.column_count,
                height: terminal_size.row_count,
            },
            should_fail_draw: false,
            draw_count,
            drawn_buffer: Arc::new(Mutex::new(Buffer::empty(ratatui::layout::Rect::new(
                0,
                0,
                terminal_size.column_count,
                terminal_size.row_count,
            )))),
        }
    }

    /// Return the cells every accepted draw wrote, shared with this backend.
    fn share_drawn_buffer(&self) -> Arc<Mutex<Buffer>> {
        Arc::clone(&self.drawn_buffer)
    }
}

impl Backend for FailingBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, cell_updates: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.draw_count.fetch_add(1, Ordering::Relaxed);
        if self.should_fail_draw {
            return Err(io::Error::other("the test backend rejected the draw"));
        }
        let mut drawn_buffer = self.drawn_buffer.lock().expect("the drawn buffer lock");
        for (column_index, row_index, drawn_cell) in cell_updates {
            drawn_buffer[(column_index, row_index)] = drawn_cell.clone();
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

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        if self.should_fail_draw {
            return Err(io::Error::other("the test backend rejected the clear"));
        }
        if clear_type == ClearType::All {
            self.drawn_buffer
                .lock()
                .expect("the drawn buffer lock")
                .reset();
        }
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

/// Wait up to five seconds for `drawn_buffer` to equal `expected_buffer` once
/// `draw_count` has reached `minimum_draw_count`, and return whether it did.
fn wait_for_drawn_buffer(
    draw_count: &AtomicUsize,
    minimum_draw_count: usize,
    drawn_buffer: &Mutex<Buffer>,
    expected_buffer: &Buffer,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    let is_expected_buffer_drawn = || {
        draw_count.load(Ordering::Relaxed) >= minimum_draw_count
            && *drawn_buffer.lock().expect("the drawn buffer lock") == *expected_buffer
    };
    while !is_expected_buffer_drawn() && Instant::now() < deadline {
        std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
    }
    is_expected_buffer_drawn()
}

/// Return the cells a fresh viewer draws for `render_snapshot` on a screen of
/// [`TEST_VIEWPORT_SIZE`].
fn build_reference_drawn_buffer(render_snapshot: &RenderSnapshot) -> Buffer {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    screen.pending_snapshot = Some(render_snapshot.clone());
    screen
        .commit_pending_snapshot(&mut client, Instant::now())
        .expect("the reference frame paints");
    screen.paint_driver.backend.buffer().clone()
}

/// Wait up to five seconds for `draw_count` to reach `expected_draw_count`,
/// and return whether it did.
fn wait_for_draw_count(draw_count: &AtomicUsize, expected_draw_count: usize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while draw_count.load(Ordering::Relaxed) < expected_draw_count && Instant::now() < deadline {
        std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
    }
    draw_count.load(Ordering::Relaxed) >= expected_draw_count
}

/// A screen drawing into memory at [`TEST_VIEWPORT_SIZE`], 80 by 24 cells.
fn build_test_screen() -> Screen<TestBackend> {
    Screen::from_backend_and_viewport_size(
        TestBackend::new(
            TEST_VIEWPORT_SIZE.column_count,
            TEST_VIEWPORT_SIZE.row_count,
        ),
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
    let image_placement_snapshot =
        ImagePlacementSnapshot::from_image_record(1, image_record, (0, 0), 1, 1)
            .expect("the test image placement is valid");

    RenderSnapshot {
        is_recovery_notice_visible: false,
        session_snapshot: SessionSnapshot {
            session_id: SessionId::new(),
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
                    is_visible: true,
                    is_suppressed: false,
                }],
                tab_size: TEST_VIEWPORT_SIZE,
                stack_headers: Vec::new(),
                layout_mode: LayoutMode::Tiled,
                is_every_pane_suppressed: false,
                gap_cell_count: 0,
            },
            tabs_metadata: vec![TabMetadata {
                tab_id,
                tab_name: String::from("tab"),
                tab_index: 0,
                is_active: true,
            }],
            floating_pane_slots: Vec::new(),
            floating_pane_ids: Vec::new(),
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
            terminal_grid_view: Some(GridView::from_grid(
                &Grid::build_blank(
                    TEST_VIEWPORT_SIZE.row_count - 2,
                    TEST_VIEWPORT_SIZE.column_count,
                    Style::default(),
                ),
                0,
            )),
            image_placement_snapshots: vec![image_placement_snapshot],
            is_reverse_video: false,
            mouse_tracking: MouseTracking::Off,
            is_alternate_scroll_enabled: false,
            is_on_alternate_screen: false,
            view_top_row_index: 0,
            selection_spans: None,
            has_selection: false,
            scrollback_metadata: ScrollbackMetadata {
                retained_line_count: 0,
            },
        }],
        client_snapshot: ClientSnapshot {
            client_id: ClientId::new(),
            viewport_size: TEST_VIEWPORT_SIZE,
            active_tab_id: tab_id,
            focused_pane_id: Some(pane_id),
            active_tab_focused_pane_id: Some(pane_id),
            lock_mode: LockMode::Normal,
            last_applied_lock_command_id: None,
            last_applied_floating_pane_move_command_id: None,
            last_applied_placement_command_id: None,
            is_mouse_selection_enabled: false,
        },
        placement_preview: None,
    }
}

/// A screen whose backend can fail its next buffer draw.
fn build_failing_screen() -> Screen<FailingBackend> {
    Screen::from_backend_and_viewport_size(
        FailingBackend::from_terminal_size(TEST_VIEWPORT_SIZE),
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
fn a_new_viewport_commits_a_new_region_revision_with_the_painted_frame() {
    let mut screen =
        Screen::from_backend_and_viewport_size(TestBackend::new(100, 30), TEST_VIEWPORT_SIZE);
    let mut client = build_test_client();
    client.set_viewport_size(Size {
        column_count: 100,
        row_count: 30,
    });

    let mouse_frame = screen
        .draw_painted_frame(
            &mut client,
            Box::new(build_test_painted_frame_with_viewport_size(Size {
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
    assert_eq!(mouse_frame.committed_regions, screen.committed_regions);
}

#[test]
fn an_in_flight_frame_uses_its_own_viewport_for_region_geometry() {
    let mut screen =
        Screen::from_backend_and_viewport_size(TestBackend::new(120, 40), TEST_VIEWPORT_SIZE);
    let mut client = build_test_client();
    let resized_viewport_size = Size {
        column_count: 120,
        row_count: 40,
    };
    client.set_viewport_size(resized_viewport_size);

    let mouse_frame = screen
        .draw_painted_frame(&mut client, Box::new(build_test_painted_frame()))
        .expect("paint");
    let expected_committed_regions = CommittedRegions::build_core(TEST_VIEWPORT_SIZE, 0);

    assert_eq!(mouse_frame.committed_regions, expected_committed_regions);
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
    let committed_regions_before = screen.committed_regions.clone();

    let sequence_opener_chord = get_sequence_opener(&client);
    assert_eq!(
        client.resolve_key(sequence_opener_chord, Instant::now()),
        KeyOutcome::Pending
    );

    let mut failed_frame = build_test_painted_frame_with_lock_mode(LockMode::Locked);
    failed_frame.session_snapshot.session_name = String::from("next");
    failed_frame.client_snapshot.is_mouse_selection_enabled = true;
    screen.paint_driver.backend.should_fail_draw = true;

    assert_eq!(
        screen.draw_painted_frame(&mut client, Box::new(failed_frame)),
        None,
        "the backend rejects the frame"
    );
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert!(!client.is_mouse_selection_enabled());
    assert_eq!(
        client.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(sequence_opener_chord))
    );
    assert_eq!(screen.shown_viewer_paint, shown_viewer_paint_before);
    assert_eq!(screen.last_snapshot, snapshot_before);
    assert_eq!(screen.committed_regions, committed_regions_before);
    assert_eq!(screen.last_window_title, "session");

    screen.paint_driver.backend.should_fail_draw = false;
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

/// Draw a Normal-mode frame focused on a new pane, open pane placement mode
/// for that pane until Esc, and draw the placement view. Returns the drawn
/// frame and the source pane.
fn draw_pane_placement_mode<B: Backend>(
    client: &mut Client,
    screen: &mut Screen<B>,
) -> (PaintedFrame, PaneId) {
    let source_pane_id = PaneId::new();
    let mut painted_frame = build_test_painted_frame_with_lock_mode(LockMode::Normal);
    painted_frame.client_snapshot.focused_pane_id = Some(source_pane_id);
    painted_frame.client_snapshot.active_tab_focused_pane_id = Some(source_pane_id);
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;
    screen
        .draw_painted_frame(client, Box::new(painted_frame.clone()))
        .expect("the first frame paints");
    assert!(client.begin_placement_mode());
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id,
            destination_tab_id: active_tab_id,
        })
    );
    screen
        .refresh(client, Some(active_tab_id))
        .expect("the placement view draws");
    (painted_frame, source_pane_id)
}

/// A preview of `source_pane_id` within tab `tab_id`, which holds
/// `[source_pane_id | target_pane_id]` solved over the test viewport
/// ([`crate::tests::build_solved_placement_snapshot`]).
fn build_side_by_side_placement_snapshot(
    source_pane_id: PaneId,
    target_pane_id: PaneId,
    tab_id: TabId,
) -> PlacementSnapshot {
    crate::tests::build_solved_placement_snapshot(
        source_pane_id,
        tab_id,
        LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![
                LayoutNode::Pane(source_pane_id),
                LayoutNode::Pane(target_pane_id),
            ],
        )),
    )
}

/// Set the target of the open pane placement mode to a swap with a new pane,
/// and return that target.
fn set_placement_swap_target(client: &mut Client) -> TiledPlacementTarget {
    let placement_target = TiledPlacementTarget::Swap {
        target_pane_id: PaneId::new(),
    };
    client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on")
        .placement_target = Some(placement_target.clone());
    placement_target
}

#[test]
fn a_frame_that_ends_pane_placement_mode_draws_once_without_it() {
    type FrameChange = fn(&mut PaintedFrame);
    let frame_changes: [(&str, FrameChange); 4] = [
        ("a new lock mode", |painted_frame| {
            painted_frame.client_snapshot.lock_mode = LockMode::Locked;
        }),
        ("mouse select turning on", |painted_frame| {
            painted_frame.client_snapshot.is_mouse_selection_enabled = true;
        }),
        ("a new active tab", |painted_frame| {
            let next_tab_id = TabId::new();
            painted_frame.client_snapshot.active_tab_id = next_tab_id;
            painted_frame.session_snapshot.active_tab_snapshot.tab_id = next_tab_id;
        }),
        ("focus on another pane", |painted_frame| {
            painted_frame.client_snapshot.focused_pane_id = Some(PaneId::new());
        }),
    ];
    for (frame_change_name, apply_frame_change) in frame_changes {
        let mut client = build_test_client();
        let mut screen = build_test_screen();
        let (mut next_frame, _) = draw_pane_placement_mode(&mut client, &mut screen);
        apply_frame_change(&mut next_frame);
        let next_active_tab_id = next_frame.client_snapshot.active_tab_id;
        let next_lock_mode = next_frame.client_snapshot.lock_mode;

        screen
            .draw_painted_frame(&mut client, Box::new(next_frame))
            .expect("the next frame paints");

        assert_eq!(
            client.placement_state.placement_mode, None,
            "{frame_change_name}"
        );
        let shown_viewer_paint = screen
            .shown_viewer_paint
            .clone()
            .expect("the next frame is shown");
        assert_eq!(
            shown_viewer_paint.lock_mode, next_lock_mode,
            "{frame_change_name}"
        );
        assert!(
            !shown_viewer_paint.chrome.is_pane_placement_visible,
            "{frame_change_name}"
        );
        assert_eq!(
            shown_viewer_paint.placement_presentation, None,
            "{frame_change_name}"
        );
        assert!(
            screen
                .refresh(&mut client, Some(next_active_tab_id))
                .is_none(),
            "{frame_change_name} draws once"
        );
    }
}

#[test]
fn a_frame_whose_new_preview_refuses_the_unconfirmed_target_draws_once_without_it() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let (next_frame, source_pane_id) = draw_pane_placement_mode(&mut client, &mut screen);
    let active_tab_id = next_frame.client_snapshot.active_tab_id;
    set_placement_swap_target(&mut client);
    screen
        .refresh(&mut client, Some(active_tab_id))
        .expect("the target draws");
    // The source pane fills the tab alone, so the swap target is in no slot.
    let placement_preview =
        PlacementPreview::Available(Box::new(crate::tests::build_solved_placement_snapshot(
            source_pane_id,
            active_tab_id,
            LayoutNode::Pane(source_pane_id),
        )));

    screen
        .draw_painted_frame_with_placement_preview(
            &mut client,
            Box::new(next_frame),
            Some(Arc::new(placement_preview)),
        )
        .expect("the next frame paints");

    assert_eq!(client.get_placement_target(), None);
    let shown_viewer_paint = screen
        .shown_viewer_paint
        .clone()
        .expect("the next frame is shown");
    assert_eq!(shown_viewer_paint.placement_target, None);
    assert!(shown_viewer_paint.chrome.is_pane_placement_visible);
    assert!(screen.refresh(&mut client, Some(active_tab_id)).is_none());
}

#[test]
fn a_frame_showing_this_viewers_committed_placement_draws_once_with_loading_preview() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let (mut next_frame, source_pane_id) = draw_pane_placement_mode(&mut client, &mut screen);
    let active_tab_id = next_frame.client_snapshot.active_tab_id;
    set_placement_swap_target(&mut client);
    let command_id = CommandId::new();
    crate::tests::record_pending_placement_command(&mut client, command_id);
    next_frame.client_snapshot.last_applied_placement_command_id = Some(command_id);

    screen
        .draw_painted_frame(&mut client, Box::new(next_frame))
        .expect("the committed frame paints");

    assert!(!client.is_placement_confirmation_pending());
    let shown_viewer_paint = screen
        .shown_viewer_paint
        .clone()
        .expect("the committed frame is shown");
    assert_eq!(shown_viewer_paint.placement_target, None);
    assert_eq!(
        shown_viewer_paint
            .placement_presentation
            .map(|placement_presentation| (
                placement_presentation.source_pane_id,
                placement_presentation.source_message.full_text
            )),
        Some((source_pane_id, "Moving pane: loading this tab".to_string()))
    );
    assert!(screen.refresh(&mut client, Some(active_tab_id)).is_none());
}

#[test]
fn a_failed_paint_keeps_the_placement_confirmation_waiting_for_its_frame() {
    let mut client = build_test_client();
    let mut screen = build_failing_screen();
    let (mut next_frame, _) = draw_pane_placement_mode(&mut client, &mut screen);
    let placement_target = set_placement_swap_target(&mut client);
    let command_id = CommandId::new();
    crate::tests::record_pending_placement_command(&mut client, command_id);
    next_frame.client_snapshot.last_applied_placement_command_id = Some(command_id);
    screen.paint_driver.backend.should_fail_draw = true;

    assert_eq!(
        screen.draw_painted_frame(&mut client, Box::new(next_frame.clone())),
        None,
        "the backend rejects the frame"
    );
    assert_eq!(client.get_pending_placement_command_id(), Some(command_id));
    assert_eq!(
        client.get_placement_target().cloned(),
        Some(placement_target)
    );

    screen.paint_driver.backend.should_fail_draw = false;
    screen
        .draw_painted_frame(&mut client, Box::new(next_frame))
        .expect("the frame paints on the next attempt");
    assert_eq!(client.get_pending_placement_command_id(), None);
    assert_eq!(client.get_placement_target(), None);
}

#[test]
fn an_image_output_failure_keeps_the_committed_text_frame() {
    let io_error = io::Error::new(io::ErrorKind::BrokenPipe, "image output closed");
    let paint_result: Result<(), terminal::PaintError<io::Error>> =
        Err(terminal::PaintError::Image(io_error));

    assert!(!is_frame_committed(paint_result));
}

#[test]
fn a_failed_native_frame_keeps_an_idle_retry_wakeup() {
    let mut screen = Screen::from_backend_with_graphics_support(
        TestBackend::new(
            TEST_VIEWPORT_SIZE.column_count,
            TEST_VIEWPORT_SIZE.row_count,
        ),
        TEST_VIEWPORT_SIZE,
        terminal::GraphicsSupport::Kitty,
        None,
    );
    screen.pending_snapshot = Some(build_native_image_snapshot());
    screen.image_output_state.fail_frame_commit();

    assert_eq!(
        screen.compute_next_image_wakeup(),
        Some(FIRST_IMAGE_OUTPUT_RETRY_DELAY_DURATION),
        "a retained snapshot must wake an idle attachment"
    );
}

#[test]
fn a_repeated_native_frame_failure_uses_capped_retry_delay() {
    let mut screen = Screen::from_backend_with_graphics_support(
        FailingBackend::from_terminal_size(TEST_VIEWPORT_SIZE),
        TEST_VIEWPORT_SIZE,
        terminal::GraphicsSupport::Kitty,
        None,
    );
    let mut client = build_test_client();
    let native_image_snapshot = build_native_image_snapshot();
    let active_tab_id = native_image_snapshot.client_snapshot.active_tab_id;
    screen.paint_driver.backend.should_fail_draw = true;

    assert_eq!(
        screen.draw_snapshot(&mut client, native_image_snapshot.clone()),
        None
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    assert_eq!(screen.pending_snapshot, Some(native_image_snapshot));
    assert_eq!(screen.native_retry_delay, Duration::from_millis(2));

    let mut expected_delay = Duration::from_millis(2);
    while expected_delay < MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION {
        let retry_at = screen
            .native_retry_at
            .expect("a failed commit has a deadline");
        assert_eq!(
            screen.compute_next_image_wakeup_at(retry_at - expected_delay),
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
            std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
        }
        expected_delay = expected_delay
            .saturating_mul(2)
            .min(MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION);
    }

    let retry_at = screen
        .native_retry_at
        .expect("the capped retry has a deadline");
    assert_eq!(
        screen.compute_next_image_wakeup_at(retry_at - MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION),
        Some(MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION)
    );
    assert_eq!(
        screen.refresh_at(&mut client, Some(active_tab_id), retry_at),
        None
    );
    assert_eq!(
        screen.native_retry_delay,
        MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION
    );

    screen.paint_driver.backend.should_fail_draw = false;
    while screen.pending_snapshot.is_some() {
        let retry_at = screen.native_retry_at.expect("the retry remains scheduled");
        screen.refresh_at(&mut client, Some(active_tab_id), retry_at);
        assert!(
            Instant::now() < deadline,
            "native image output did not recover after the writer recovered"
        );
        std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
    }
    assert_eq!(
        screen.native_retry_delay,
        FIRST_IMAGE_OUTPUT_RETRY_DELAY_DURATION
    );
}

#[test]
fn a_native_retry_delay_resets_when_output_connection_resets() {
    let mut screen = Screen::from_backend_with_graphics_support(
        TestBackend::new(
            TEST_VIEWPORT_SIZE.column_count,
            TEST_VIEWPORT_SIZE.row_count,
        ),
        TEST_VIEWPORT_SIZE,
        terminal::GraphicsSupport::Kitty,
        None,
    );
    screen.native_retry_delay = MAX_IMAGE_OUTPUT_RETRY_DELAY_DURATION;
    screen.native_retry_at = Some(Instant::now());

    screen.reset_connection();

    assert_eq!(
        screen.native_retry_delay,
        FIRST_IMAGE_OUTPUT_RETRY_DELAY_DURATION
    );
    assert_eq!(screen.native_retry_at, None);
}

#[test]
fn a_pending_text_frame_does_not_schedule_an_image_wakeup() {
    let mut screen = build_test_screen();
    screen.pending_snapshot = Some(build_render_snapshot(&build_received_painted_frame(
        build_test_painted_frame(),
    )));

    assert_eq!(screen.compute_next_image_wakeup(), None);
}

#[test]
fn a_native_image_frame_paints_its_text_at_once_and_its_image_after_the_worker_wakes_the_loop() {
    let (image_output_ready_sender, image_output_ready_receiver) = mpsc::channel();
    let mut screen = Screen::with_graphics_support(
        terminal::PaintDriver::from_backend(
            TestBackend::new(
                TEST_VIEWPORT_SIZE.column_count,
                TEST_VIEWPORT_SIZE.row_count,
            ),
            TEST_VIEWPORT_SIZE,
        ),
        terminal::TerminalOutput::from_terminal_sink(Box::new(io::sink())),
        TEST_VIEWPORT_SIZE,
        terminal::GraphicsSupport::Kitty,
        None,
        Arc::new(move || {
            let _ = image_output_ready_sender.send(());
        }),
    );
    let mut client = build_test_client();
    let native_image_snapshot = build_native_image_snapshot();
    let active_tab_id = native_image_snapshot.client_snapshot.active_tab_id;
    let image_placement_key = (
        native_image_snapshot.pane_snapshots[0].pane_id,
        native_image_snapshot.pane_snapshots[0].image_placement_snapshots[0].get_placement_id(),
    );

    let expected_session_snapshot = native_image_snapshot.session_snapshot.clone();
    assert_eq!(
        screen
            .draw_snapshot(&mut client, native_image_snapshot)
            .map(|mouse_frame| mouse_frame.session_snapshot),
        Some(expected_session_snapshot.clone()),
        "the text of the frame waited for its image"
    );
    assert_eq!(screen.pending_snapshot, None);
    assert_eq!(screen.image_output_state.list_shown_placement_keys(), []);
    assert_eq!(screen.compute_next_image_wakeup(), None);

    image_output_ready_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the worker ends the upload");
    assert_eq!(
        screen
            .refresh(&mut client, Some(active_tab_id))
            .map(|mouse_frame| mouse_frame.session_snapshot),
        Some(expected_session_snapshot)
    );
    assert_eq!(
        screen.image_output_state.list_shown_placement_keys(),
        [image_placement_key]
    );
    assert_eq!(screen.refresh(&mut client, Some(active_tab_id)), None);
    assert_eq!(screen.compute_next_image_wakeup(), None);
}

#[test]
fn native_images_return_after_scrolling_away_for_each_output_protocol() {
    let cell_size = PixelCellSize::from_pixel_dimensions(1, 1).expect("test cell size");
    let output_protocols = [
        terminal::GraphicsSupport::Kitty,
        terminal::GraphicsSupport::Iterm,
        terminal::GraphicsSupport::Sixel {
            palette_color_count: 256,
            maximum_pixel_width: None,
            maximum_pixel_height: None,
        },
    ];

    for graphics_support in output_protocols {
        let mut screen = Screen::from_backend_with_graphics_support(
            TestBackend::new(
                TEST_VIEWPORT_SIZE.column_count,
                TEST_VIEWPORT_SIZE.row_count,
            ),
            TEST_VIEWPORT_SIZE,
            graphics_support,
            Some(cell_size),
        );
        let mut client = build_test_client();
        let image_render_snapshot = build_native_image_snapshot();
        let active_tab_id = image_render_snapshot.client_snapshot.active_tab_id;
        let image_placement_key = (
            image_render_snapshot.pane_snapshots[0].pane_id,
            image_render_snapshot.pane_snapshots[0].image_placement_snapshots[0].get_placement_id(),
        );
        let mut scrolled_away_render_snapshot = image_render_snapshot.clone();
        for pane_snapshot in &mut scrolled_away_render_snapshot.pane_snapshots {
            pane_snapshot.image_placement_snapshots.clear();
        }
        let expected_session_snapshot = image_render_snapshot.session_snapshot.clone();

        assert_eq!(
            screen
                .draw_snapshot(&mut client, image_render_snapshot.clone())
                .map(|mouse_frame| mouse_frame.session_snapshot),
            Some(expected_session_snapshot.clone()),
            "{graphics_support:?} text waited for the image"
        );
        wait_for_native_image_output(&mut screen, &mut client, active_tab_id);
        assert_eq!(
            screen.image_output_state.list_shown_placement_keys(),
            [image_placement_key],
            "{graphics_support:?}"
        );

        assert_eq!(
            screen
                .draw_snapshot(&mut client, scrolled_away_render_snapshot)
                .map(|mouse_frame| mouse_frame.session_snapshot),
            Some(expected_session_snapshot.clone()),
            "{graphics_support:?} must commit the frame with no visible image"
        );
        assert_eq!(
            screen.image_output_state.list_shown_placement_keys(),
            [],
            "{graphics_support:?}"
        );

        assert_eq!(
            screen
                .draw_snapshot(&mut client, image_render_snapshot)
                .map(|mouse_frame| mouse_frame.session_snapshot),
            Some(expected_session_snapshot.clone()),
            "{graphics_support:?} text waited for the returning image"
        );
        wait_for_native_image_output(&mut screen, &mut client, active_tab_id);
        assert_eq!(
            screen.image_output_state.list_shown_placement_keys(),
            [image_placement_key],
            "{graphics_support:?}"
        );
    }
}

/// Refresh `screen` until its image output has no request running or
/// waiting, and no result left to show.
fn wait_for_native_image_output(
    screen: &mut Screen<TestBackend>,
    client: &mut Client,
    active_tab_id: TabId,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while screen.image_output_state.is_work_pending() {
        screen.refresh(client, Some(active_tab_id));
        assert!(
            Instant::now() < deadline,
            "native image output did not settle"
        );
        std::thread::sleep(crate::tests::TEST_POLL_INTERVAL_DURATION);
    }
    assert_eq!(screen.pending_snapshot, None);
    assert_eq!(screen.compute_next_image_wakeup(), None);
}

#[test]
fn confirming_keyboard_placement_repaints_without_resize() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let painted_frame = build_test_painted_frame();
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;
    screen
        .draw_painted_frame(&mut client, Box::new(painted_frame))
        .expect("paint the committed frame");

    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    crate::tests::hold_placement_snapshot(
        &mut client,
        build_side_by_side_placement_snapshot(source_pane_id, target_pane_id, active_tab_id),
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap { target_pane_id }),
        pending_placement_command_id: None,
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
    screen.refresh(&mut client, Some(active_tab_id));
    let paint_before_confirmation = screen
        .shown_viewer_paint
        .clone()
        .expect("the placement draft is painted");

    assert_eq!(
        client.submit_placement_command(),
        Some(crate::tests::build_expected_confirm_placement(
            &client,
            source_pane_id,
            TiledPlacementTarget::Swap { target_pane_id },
        ))
    );
    assert!(client.is_placement_confirmation_pending());
    screen.refresh(&mut client, Some(active_tab_id));

    let paint_after_confirmation = screen
        .shown_viewer_paint
        .as_ref()
        .expect("the confirmation repaint is recorded");
    assert_eq!(
        paint_before_confirmation
            .placement_presentation
            .map(|placement_presentation| placement_presentation.source_message),
        Some(PanePlacementMessage {
            full_text: "Moving pane will land here".to_string(),
            compact_text: "Moving pane",
            detail_text: Some("Enter to place · Esc to cancel".to_string()),
        })
    );
    assert_eq!(
        paint_after_confirmation
            .placement_presentation
            .as_ref()
            .map(|placement_presentation| (
                placement_presentation.source_pane_id,
                &placement_presentation.source_message
            )),
        Some((
            source_pane_id,
            &PanePlacementMessage {
                full_text: "Placing this pane…".to_string(),
                compact_text: "Placing…",
                detail_text: Some("Waiting for session".to_string()),
            }
        ))
    );

    let mut authoritative_frame = build_test_painted_frame();
    authoritative_frame
        .session_snapshot
        .active_tab_snapshot
        .tab_id = active_tab_id;
    authoritative_frame.client_snapshot.active_tab_id = active_tab_id;
    authoritative_frame
        .client_snapshot
        .last_applied_placement_command_id = client.get_pending_placement_command_id();
    screen
        .draw_painted_frame(&mut client, Box::new(authoritative_frame))
        .expect("paint the authoritative placement result");
    screen.refresh(&mut client, Some(active_tab_id));

    assert!(!client.is_placement_confirmation_pending());
    assert_eq!(client.get_placement_target(), None);
    assert_eq!(
        screen
            .shown_viewer_paint
            .as_ref()
            .and_then(|viewer_paint| viewer_paint.placement_target.as_ref()),
        None
    );
}

#[test]
fn mouse_placement_confirmation_clears_confirming_preview_without_resize() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let painted_frame = build_test_painted_frame();
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;
    let session_id = painted_frame.session_snapshot.session_id;
    screen
        .draw_painted_frame(&mut client, Box::new(painted_frame))
        .expect("paint the committed frame");

    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    crate::tests::hold_placement_snapshot(
        &mut client,
        build_side_by_side_placement_snapshot(source_pane_id, target_pane_id, active_tab_id),
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap { target_pane_id }),
        pending_placement_command_id: None,
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilDragEnds;
    client
        .client_config
        .should_stay_in_pane_placement_mode_after_placement = false;
    screen.refresh(&mut client, Some(active_tab_id));

    assert_eq!(
        client.submit_placement_command(),
        Some(crate::tests::build_expected_confirm_placement(
            &client,
            source_pane_id,
            TiledPlacementTarget::Swap { target_pane_id },
        ))
    );
    assert!(!client.is_placement_mode_active());
    assert!(client.is_placement_confirmation_pending());
    screen.refresh(&mut client, Some(active_tab_id));

    assert_eq!(
        screen
            .shown_viewer_paint
            .as_ref()
            .and_then(|viewer_paint| viewer_paint.placement_presentation.as_ref())
            .map(|placement_presentation| placement_presentation.source_message.full_text.as_str()),
        Some("Placing this pane…")
    );
    assert_eq!(client.get_active_input_mode(), LockMode::Normal);

    let mut authoritative_frame = build_test_painted_frame();
    authoritative_frame.session_snapshot.session_id = session_id;
    authoritative_frame
        .session_snapshot
        .active_tab_snapshot
        .tab_id = active_tab_id;
    authoritative_frame.client_snapshot.client_id = client.get_client_id();
    authoritative_frame.client_snapshot.active_tab_id = active_tab_id;
    authoritative_frame
        .client_snapshot
        .last_applied_placement_command_id = client.get_pending_placement_command_id();
    screen
        .draw_painted_frame(&mut client, Box::new(authoritative_frame))
        .expect("paint the accepted mouse placement frame");
    screen.refresh(&mut client, Some(active_tab_id));

    assert!(!client.is_placement_confirmation_pending());
    assert!(!client.is_pane_placement_visible());
    assert_eq!(
        screen
            .shown_viewer_paint
            .as_ref()
            .and_then(|viewer_paint| viewer_paint.placement_presentation.as_ref()),
        None
    );
}

#[test]
fn enter_submits_keyboard_placement_and_repaints_without_resize() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let painted_frame = build_test_painted_frame();
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;
    screen
        .draw_painted_frame(&mut client, Box::new(painted_frame))
        .expect("paint the committed frame");

    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    client.set_frame_view(
        active_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![active_tab_id],
    );
    crate::tests::hold_placement_snapshot(
        &mut client,
        build_side_by_side_placement_snapshot(source_pane_id, target_pane_id, active_tab_id),
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap { target_pane_id }),
        pending_placement_command_id: None,
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
    screen.refresh(&mut client, Some(active_tab_id));

    let client_id = client.get_client_id();
    let mut control_socket = build_test_control_socket();
    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Named(NamedKey::Enter),
            )),
        },
        Instant::now(),
    );
    screen.refresh(&mut client, Some(active_tab_id));

    control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let placement_request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the placement command");
    let PlacementInputAction::ConfirmPlacement {
        command_id,
        source_pane_id: confirmed_source_pane_id,
        placement_target,
        expected_placed_tab_layout,
    } = crate::tests::build_expected_confirm_placement(
        &client,
        source_pane_id,
        TiledPlacementTarget::Swap { target_pane_id },
    )
    else {
        panic!("the expected confirmation is a confirmation");
    };
    assert_eq!(
        placement_request.request_kind,
        IpcRequestKind::ConfirmPanePlacement {
            command_id,
            source_pane_id: confirmed_source_pane_id,
            placement_target,
            expected_placed_tab_layout: *expected_placed_tab_layout,
        }
    );
    assert!(client.is_placement_confirmation_pending());
    assert_eq!(
        screen
            .shown_viewer_paint
            .as_ref()
            .and_then(|viewer_paint| viewer_paint.placement_presentation.as_ref())
            .map(|placement_presentation| placement_presentation.source_message.full_text.as_str()),
        Some("Placing this pane…")
    );
}

#[test]
fn mouse_pickup_previews_and_focuses_the_dragged_pane() {
    let initial_focused_pane_id = PaneId::new();
    let dragged_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[
        build_plain_mouse_pane(initial_focused_pane_id),
        build_plain_mouse_pane(dragged_pane_id),
    ]);
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let screen_pane_content_rect =
        koshi_renderer::find_pane_content_rect(mouse_frame.build_frame_layout(), dragged_pane_id)
            .expect("the dragged pane has visible content");
    let placement_handle_position = Point {
        column: screen_pane_content_rect.origin.column,
        row: screen_pane_content_rect.origin.row - 1,
    };
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    client.set_frame_view(
        active_tab_id,
        Some(initial_focused_pane_id),
        Some(initial_focused_pane_id),
        vec![active_tab_id],
    );
    client.handle_mouse(
        build_mouse_motion(placement_handle_position),
        &mouse_frame,
        Instant::now(),
    );
    let mouse_frame = MouseFrame {
        viewer_chrome: client.build_viewer_chrome(active_tab_id),
        ..mouse_frame
    };
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    let mut pending_mouse_actions = Vec::new();

    assert_eq!(
        handle_terminal_input_event(
            &mut client,
            &mut uplink,
            &mut cell_size_query,
            Some(&mouse_frame),
            TerminalInput {
                runtime_event: RuntimeEvent::MouseInput {
                    client_id,
                    mouse_input: build_mouse_press(placement_handle_position),
                },
                read_at: Instant::now(),
            },
            &mut Vec::new(),
            &mut pending_mouse_actions,
        ),
        ControlFlow::Continue(())
    );
    assert_eq!(client.get_placement_source_pane_id(), Some(dragged_pane_id));
    assert_eq!(
        pending_mouse_actions,
        Vec::new(),
        "pane placement takes the press"
    );

    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id: dragged_pane_id,
            destination_tab_id: active_tab_id,
        })
    );

    let focus_request = request_receiver
        .try_recv()
        .expect("read the source focus command");
    assert_eq!(focus_request.request_id, FIRST_POST_ATTACH_REQUEST_ID);
    let IpcRequestKind::SubmitCommand(focus_envelope) = focus_request.request_kind else {
        panic!("the mouse pickup focuses its dragged pane");
    };
    assert_eq!(
        focus_envelope.command_source,
        CommandSource::Mouse { client_id }
    );
    assert_eq!(
        focus_envelope.command,
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(dragged_pane_id),
            client_id: Some(client_id),
        })
    );
    let sentinel_request_id = uplink.send_request(IpcRequestKind::Discovery);
    assert_eq!(
        request_receiver
            .try_recv()
            .expect("read the request after pickup"),
        IpcRequest {
            request_id: sentinel_request_id,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn attachment_commits_entered_pane_swap_without_waiting_for_resize() {
    assert_attachment_commits_pane_swap_without_waiting_for_resize(
        PlacementModeLifetime::UntilCancelled,
        true,
    );
}

#[test]
fn attachment_commits_mouse_released_pane_swap_without_waiting_for_resize() {
    assert_attachment_commits_pane_swap_without_waiting_for_resize(
        PlacementModeLifetime::UntilDragEnds,
        true,
    );
}

#[test]
fn attachment_commits_entered_pane_swap_and_ends_pane_placement_mode_when_staying_is_off() {
    assert_attachment_commits_pane_swap_without_waiting_for_resize(
        PlacementModeLifetime::UntilCancelled,
        false,
    );
}

#[test]
fn attachment_commits_mouse_released_pane_swap_and_ends_pane_placement_mode_when_staying_is_off() {
    assert_attachment_commits_pane_swap_without_waiting_for_resize(
        PlacementModeLifetime::UntilDragEnds,
        false,
    );
}

fn assert_attachment_commits_pane_swap_without_waiting_for_resize(
    placement_mode_lifetime: PlacementModeLifetime,
    should_stay_in_pane_placement_mode_after_placement: bool,
) {
    let mut client = build_test_client();
    client
        .client_config
        .should_stay_in_pane_placement_mode_after_placement =
        should_stay_in_pane_placement_mode_after_placement;
    client.client_config.should_reduce_motion = true;
    let client_id = client.get_client_id();
    let session_id = SessionId::new();
    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    let mut initial_frame = build_test_painted_frame();
    let active_tab_id = initial_frame.client_snapshot.active_tab_id;
    let source_outer_rect = Rect::from_origin_and_size(
        Point { column: 0, row: 0 },
        Size {
            column_count: 39,
            row_count: 22,
        },
    );
    let target_outer_rect = Rect::from_origin_and_size(
        Point { column: 41, row: 0 },
        Size {
            column_count: 39,
            row_count: 22,
        },
    );
    let build_frame_slot = |pane_id, outer_rect| FrameSlot {
        pane_id,
        outer_rect,
        content_rect: Some(outer_rect.compute_inner_with_border()),
        is_visible: true,
        is_suppressed: false,
    };
    initial_frame.session_snapshot.session_id = session_id;
    initial_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots = vec![
        build_frame_slot(source_pane_id, source_outer_rect),
        build_frame_slot(target_pane_id, target_outer_rect),
    ];
    initial_frame.client_snapshot.client_id = client_id;
    initial_frame.client_snapshot.focused_pane_id = Some(source_pane_id);

    let mut committed_frame = initial_frame.clone();
    committed_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .outer_rect = target_outer_rect;
    committed_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .content_rect = Some(target_outer_rect.compute_inner_with_border());
    committed_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[1]
        .outer_rect = source_outer_rect;
    committed_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[1]
        .content_rect = Some(source_outer_rect.compute_inner_with_border());

    // Each frame previews the tab `[first | second]` of `leaf_pane_ids`, with
    // the slots that frame draws in that leaf order.
    let build_frame_placement_snapshot =
        |painted_frame: &PaintedFrame, leaf_pane_ids: [PaneId; 2]| {
            let mut placement_snapshot = crate::tests::build_solved_placement_snapshot(
                source_pane_id,
                active_tab_id,
                LayoutNode::Split(SplitNode::with_equal_weights(
                    SplitDirection::Horizontal,
                    leaf_pane_ids.map(LayoutNode::Pane).to_vec(),
                )),
            );
            let frame_pane_slots =
                build_render_snapshot(&build_received_painted_frame(painted_frame.clone()))
                    .session_snapshot
                    .active_tab_snapshot
                    .pane_slots;
            get_source_tab_snapshot_mut(&mut placement_snapshot)
                .tab_snapshot
                .pane_slots = leaf_pane_ids
                .iter()
                .map(|pane_id| {
                    frame_pane_slots
                        .iter()
                        .find(|pane_slot| pane_slot.pane_id == *pane_id)
                        .cloned()
                        .expect("the frame draws the pane")
                })
                .collect();
            placement_snapshot
        };
    let initial_placement_snapshot =
        build_frame_placement_snapshot(&initial_frame, [source_pane_id, target_pane_id]);
    let committed_placement_snapshot =
        build_frame_placement_snapshot(&committed_frame, [target_pane_id, source_pane_id]);
    let placement_target = TiledPlacementTarget::Swap { target_pane_id };
    let expected_placed_tab_layout = initial_placement_snapshot
        .build_placed_preview(&placement_target)
        .expect("the tab takes the swap")
        .placed_tab_layout;

    client.set_session_id(session_id);
    client.set_frame_view(
        active_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![active_tab_id],
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: (placement_mode_lifetime == PlacementModeLifetime::UntilCancelled)
            .then_some(placement_target.clone()),
        pending_placement_command_id: None,
    });
    client.placement_state.placement_mode_lifetime = placement_mode_lifetime;
    let source_drag_position = Point {
        column: source_outer_rect.origin.column + 2,
        row: source_outer_rect.origin.row + 2,
    };
    let target_drag_position = Point {
        column: target_outer_rect.origin.column + 2,
        row: target_outer_rect.origin.row + 2,
    };
    if placement_mode_lifetime == PlacementModeLifetime::UntilDragEnds {
        client.begin_placement_drag(source_drag_position, false);
    }
    let placement_confirmation_events = match placement_mode_lifetime {
        PlacementModeLifetime::UntilCancelled => vec![RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(KeyChord::from_parts(
                BindingModifierFlags::NONE,
                Key::Named(NamedKey::Enter),
            )),
        }],
        PlacementModeLifetime::UntilDragEnds => vec![
            RuntimeEvent::MouseInput {
                client_id,
                mouse_input: build_mouse_drag(target_drag_position),
            },
            RuntimeEvent::MouseInput {
                client_id,
                mouse_input: build_mouse_release(target_drag_position),
            },
        ],
    };
    let expected_draw_count = match placement_mode_lifetime {
        PlacementModeLifetime::UntilCancelled => 3,
        PlacementModeLifetime::UntilDragEnds => 4,
    };

    let draw_count = Arc::new(AtomicUsize::new(0));
    let mut screen = Screen::from_backend_and_viewport_size(
        FailingBackend::from_terminal_size_with_draw_count(
            TEST_VIEWPORT_SIZE,
            Arc::clone(&draw_count),
        ),
        TEST_VIEWPORT_SIZE,
    );
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    let producer_sender = incoming_sender.clone();
    let producer_draw_count = Arc::clone(&draw_count);
    let producer_placement_target = placement_target.clone();
    let producer_thread = thread::spawn(move || -> Result<(bool, CommandId), String> {
        let quit_with = |failure_text: String| {
            let _ = producer_sender.send(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
            Err(failure_text)
        };
        producer_sender
            .send(Incoming::Painted {
                connection_index: INITIAL_CONNECTION_INDEX,
                received_painted_frame: Box::new(
                    build_received_painted_frame_with_placement_preview(
                        initial_frame,
                        PlacementPreview::Available(Box::new(initial_placement_snapshot)),
                    ),
                ),
            })
            .map_err(|send_error| send_error.to_string())?;
        if !wait_for_draw_count(&producer_draw_count, 1) {
            return quit_with(String::from("the initial frame was not drawn"));
        }
        let initial_request_kinds = (0..2)
            .map(|_| {
                request_receiver
                    .recv_timeout(Duration::from_secs(5))
                    .map(|ipc_request| ipc_request.request_kind)
            })
            .collect::<Vec<_>>();
        let expected_initial_request_kinds = vec![
            Ok(IpcRequestKind::SetPanePlacementPreview {
                placement_target: Some(PanePlacementPreviewTarget {
                    source_pane_id,
                    destination_tab_id: active_tab_id,
                }),
            }),
            Ok(IpcRequestKind::PaintedUpdate { update_sequence: 1 }),
        ];
        if initial_request_kinds != expected_initial_request_kinds {
            return quit_with(format!(
                "the pass that drew the initial frame named the preview target and reported \
                 the frame painted: {initial_request_kinds:?}"
            ));
        }
        for (placement_event_index, placement_confirmation_event) in
            placement_confirmation_events.into_iter().enumerate()
        {
            producer_sender
                .send(build_input_incoming(
                    placement_confirmation_event,
                    Instant::now(),
                ))
                .map_err(|send_error| send_error.to_string())?;
            if placement_mode_lifetime == PlacementModeLifetime::UntilDragEnds
                && placement_event_index == 0
                && !wait_for_draw_count(&producer_draw_count, 2)
            {
                return quit_with(String::from("the dragged target was not painted"));
            }
        }

        let placement_request = match request_receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(placement_request) => placement_request,
            Err(request_error) => return quit_with(request_error.to_string()),
        };
        let IpcRequestKind::ConfirmPanePlacement {
            command_id,
            source_pane_id: confirmed_source_pane_id,
            placement_target: confirmed_placement_target,
            expected_placed_tab_layout: confirmed_placed_tab_layout,
        } = placement_request.request_kind
        else {
            return quit_with(String::from("the placement input confirmed no placement"));
        };
        let is_expected_request = placement_request.request_id == FIRST_POST_ATTACH_REQUEST_ID + 2
            && confirmed_source_pane_id == source_pane_id
            && confirmed_placement_target == producer_placement_target
            && confirmed_placed_tab_layout == expected_placed_tab_layout;
        producer_sender
            .send(build_session_event_incoming(
                SessionEvent::PanePlacementCommitted {
                    command_id,
                    source_pane_id,
                    source_tab_id: Some(active_tab_id),
                    destination_tab_id: Some(active_tab_id),
                    placement_target: PanePlacementTarget::Tiled(producer_placement_target),
                },
            ))
            .map_err(|send_error| send_error.to_string())?;
        committed_frame
            .client_snapshot
            .last_applied_placement_command_id = Some(command_id);
        producer_sender
            .send(Incoming::Painted {
                connection_index: INITIAL_CONNECTION_INDEX,
                received_painted_frame: Box::new(
                    build_received_painted_frame_with_placement_preview(
                        committed_frame,
                        PlacementPreview::Available(Box::new(committed_placement_snapshot)),
                    ),
                ),
            })
            .map_err(|send_error| send_error.to_string())?;
        if !wait_for_draw_count(&producer_draw_count, expected_draw_count) {
            return quit_with(String::from("the committed frame was not drawn"));
        }
        producer_sender
            .send(build_session_event_incoming(SessionEvent::Detached))
            .map_err(|send_error| send_error.to_string())?;
        Ok((is_expected_request, command_id))
    });
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    let mut terminal_mode_bytes = Vec::new();
    let attachment_ending = run_attachment(
        &build_local_home(),
        session_id,
        client_id,
        ConnectionToken::generate(),
        None,
        &mut client,
        &mut screen,
        &mut uplink,
        TEXT_ONLY_ATTACH_SETTINGS,
        &mut cell_size_query,
        &mut terminal_mode_bytes,
        incoming_sender,
        incoming_receiver,
    );
    let (is_expected_request, command_id) = producer_thread
        .join()
        .expect("the frame producer finished")
        .expect("the attachment confirmed and painted the placement");

    assert_eq!(attachment_ending, AttachmentEnding::Detached);
    assert_eq!(
        terminal_mode_bytes,
        match (
            placement_mode_lifetime,
            should_stay_in_pane_placement_mode_after_placement,
        ) {
            (PlacementModeLifetime::UntilCancelled, true) => b"\x1b[>1s".as_slice(),
            (PlacementModeLifetime::UntilDragEnds, true) => {
                b"\x1b[>1s\x1b[>0s\x1b[>1s".as_slice()
            }
            (_, false) => b"\x1b[>1s\x1b[>0s".as_slice(),
        }
    );
    assert!(
        is_expected_request,
        "the placement input confirms the pane swap with the tab its preview draws after it"
    );
    assert_eq!(
        draw_count.load(Ordering::Relaxed),
        expected_draw_count,
        "the frame, the confirmation, and the committed layout each draw once, without resize"
    );
    assert!(!client.is_placement_confirmation_pending());
    let shown_placement_presentation = screen
        .shown_viewer_paint
        .as_ref()
        .and_then(|viewer_paint| viewer_paint.placement_presentation.as_ref());
    if should_stay_in_pane_placement_mode_after_placement {
        assert_eq!(
            shown_placement_presentation.map(|placement_presentation| (
                placement_presentation.source_pane_id,
                placement_presentation.source_message.full_text.as_str()
            )),
            Some((source_pane_id, "Moving pane: choose a destination"))
        );
        assert!(client.is_placement_mode_active());
        assert_eq!(
            client.placement_state.placement_mode_lifetime,
            PlacementModeLifetime::UntilCancelled
        );
    } else {
        assert_eq!(shown_placement_presentation, None);
        assert!(client.placement_state.placement_mode.is_none());
    }
    let shown_render_snapshot = screen
        .last_snapshot
        .as_ref()
        .expect("the accepted frame remains on the screen");
    assert_eq!(
        shown_render_snapshot
            .client_snapshot
            .last_applied_placement_command_id,
        Some(command_id)
    );
    assert_eq!(
        shown_render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .pane_slots[0]
            .outer_rect,
        target_outer_rect
    );
    assert_eq!(
        shown_render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .pane_slots[1]
            .outer_rect,
        source_outer_rect
    );
}

#[test]
fn placement_presentation_describes_both_swap_destinations() {
    let mut client = build_test_client();
    let mut render_snapshot =
        build_render_snapshot(&build_received_painted_frame(build_test_painted_frame()));
    let active_tab_id = render_snapshot.client_snapshot.active_tab_id;
    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    render_snapshot.session_snapshot.tabs_metadata = vec![TabMetadata {
        tab_id: active_tab_id,
        tab_name: String::from("main"),
        tab_index: 0,
        is_active: true,
    }];
    let build_pane_snapshot = |pane_id, pane_title| PaneSnapshot {
        pane_id,
        pane_title: Some(String::from(pane_title)),
        cursor_snapshot: CursorSnapshot {
            row_index: 0,
            column_index: 0,
            is_visible: false,
            is_blinking: false,
            shape: None,
        },
        terminal_grid_view: None,
        image_placement_snapshots: Vec::new(),
        is_reverse_video: false,
        mouse_tracking: MouseTracking::Off,
        is_alternate_scroll_enabled: false,
        is_on_alternate_screen: false,
        view_top_row_index: 0,
        selection_spans: None,
        has_selection: false,
        scrollback_metadata: ScrollbackMetadata {
            retained_line_count: 0,
        },
    };
    render_snapshot.pane_snapshots = vec![
        build_pane_snapshot(source_pane_id, "/home/user/project"),
        build_pane_snapshot(target_pane_id, "nvim"),
    ];

    client.set_frame_view(
        active_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![active_tab_id],
    );
    crate::tests::hold_placement_snapshot(
        &mut client,
        build_side_by_side_placement_snapshot(source_pane_id, target_pane_id, active_tab_id),
    );
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Swap { target_pane_id }),
        pending_placement_command_id: None,
    });

    let placement_presentation = build_pane_placement_presentation(&client, &render_snapshot)
        .expect("the pane placement has a visible preview");
    assert_eq!(
        placement_presentation.source_message.full_text,
        "Moving pane will land here"
    );
    assert_eq!(
        placement_presentation.source_message.compact_text,
        "Moving pane"
    );
    assert_eq!(placement_presentation.source_pane_id, source_pane_id);
    assert_eq!(placement_presentation.target_pane_ids, vec![target_pane_id]);
    assert_eq!(
        placement_presentation
            .target_message
            .as_ref()
            .map(|pane_message| pane_message.full_text.as_str()),
        Some("Other pane will move here")
    );
    assert_eq!(
        placement_presentation
            .target_message
            .as_ref()
            .map(|pane_message| pane_message.compact_text),
        Some("Other pane")
    );
    assert_eq!(
        placement_presentation
            .target_message
            .as_ref()
            .and_then(|pane_message| pane_message.detail_text.as_deref()),
        Some("Into the moving pane's old place")
    );
}

#[test]
fn placement_presentation_describes_a_stack_target_and_names_the_tab_a_floating_pane_docks_into() {
    let mut client = build_test_client();
    let mut render_snapshot =
        build_render_snapshot(&build_received_painted_frame(build_test_painted_frame()));
    let active_tab_id = render_snapshot.client_snapshot.active_tab_id;
    let source_pane_id = PaneId::new();
    let target_pane_id = PaneId::new();
    render_snapshot.session_snapshot.tabs_metadata = vec![TabMetadata {
        tab_id: active_tab_id,
        tab_name: String::from("main"),
        tab_index: 0,
        is_active: true,
    }];
    client.set_frame_view(
        active_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![active_tab_id],
    );
    let mut placement_snapshot =
        build_side_by_side_placement_snapshot(source_pane_id, target_pane_id, active_tab_id);
    crate::tests::hold_placement_snapshot(&mut client, placement_snapshot.clone());
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Stack { target_pane_id }),
        pending_placement_command_id: None,
    });
    let expected_target_message = Some(PanePlacementMessage {
        full_text: "Other pane will stack behind it".to_string(),
        compact_text: "Other pane",
        detail_text: None,
    });

    let placement_presentation = build_pane_placement_presentation(&client, &render_snapshot)
        .expect("the pane placement has a visible preview");
    assert_eq!(
        placement_presentation.source_message,
        PanePlacementMessage {
            full_text: "Moving pane will open in this stack".to_string(),
            compact_text: "Moving pane",
            detail_text: Some("Enter to place · Esc to cancel".to_string()),
        }
    );
    assert_eq!(placement_presentation.target_pane_ids, vec![target_pane_id]);
    assert_eq!(
        placement_presentation.target_message,
        expected_target_message
    );

    // A floating source has no tab of its own: the text names the tab it
    // docks into.
    float_placement_source_pane(&mut placement_snapshot);
    crate::tests::hold_placement_snapshot(&mut client, placement_snapshot);
    if let Some(placement_mode) = client.placement_state.placement_mode.as_mut() {
        placement_mode.source_tab_id = None;
    }
    let placement_presentation = build_pane_placement_presentation(&client, &render_snapshot)
        .expect("the pane placement has a visible preview");
    assert_eq!(
        placement_presentation.source_message.detail_text.as_deref(),
        Some("In main")
    );
    assert_eq!(
        placement_presentation.target_message,
        expected_target_message
    );
}

#[test]
fn placement_presentation_names_group_tab_and_cross_tab_results() {
    let mut client = build_test_client();
    let mut render_snapshot =
        build_render_snapshot(&build_received_painted_frame(build_test_painted_frame()));
    let source_tab_id = render_snapshot.client_snapshot.active_tab_id;
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    let first_target_pane_id = PaneId::new();
    let second_target_pane_id = PaneId::new();
    render_snapshot
        .session_snapshot
        .tabs_metadata
        .push(TabMetadata {
            tab_id: destination_tab_id,
            tab_name: "logs".to_string(),
            tab_index: 1,
            is_active: false,
        });
    client.set_frame_view(
        source_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![source_tab_id, destination_tab_id],
    );
    let mut placement_snapshot =
        crate::tests::build_test_placement_snapshot(source_pane_id, source_tab_id, source_tab_id);
    placement_snapshot.destination_tab_snapshot = None;
    placement_snapshot.swap_tab_sizes.clear();
    for target_pane_id in [first_target_pane_id, second_target_pane_id] {
        let mut target_slot = get_source_tab_snapshot_mut(&mut placement_snapshot)
            .tab_snapshot
            .pane_slots[0]
            .clone();
        target_slot.pane_id = target_pane_id;
        get_source_tab_snapshot_mut(&mut placement_snapshot)
            .tab_snapshot
            .pane_slots
            .push(target_slot);
    }
    crate::tests::hold_placement_snapshot(&mut client, placement_snapshot);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Down,
        placement_target: Some(TiledPlacementTarget::Split {
            destination_tab_id: source_tab_id,
            anchor: PanePlacementAnchor::Group(vec![first_target_pane_id, second_target_pane_id]),
            direction: Direction::Down,
        }),
        pending_placement_command_id: None,
    });
    let group_presentation =
        build_pane_placement_presentation(&client, &render_snapshot).expect("group preview");
    assert_eq!(
        group_presentation.target_pane_ids,
        vec![first_target_pane_id, second_target_pane_id]
    );
    assert_eq!(
        group_presentation.source_message.full_text,
        "Moving pane will insert below"
    );
    assert_eq!(
        group_presentation.source_message.compact_text,
        "Moving pane"
    );
    assert_eq!(
        group_presentation
            .target_message
            .as_ref()
            .map(|pane_message| pane_message.full_text.as_str()),
        Some("Other panes will make room below")
    );
    assert_eq!(
        group_presentation
            .target_message
            .as_ref()
            .map(|pane_message| pane_message.compact_text),
        Some("Other panes")
    );
    assert_eq!(
        group_presentation.source_message.detail_text.as_deref(),
        Some("Enter to place · Esc to cancel")
    );

    client.begin_placement_drag(Point { column: 1, row: 1 }, false);
    let dragged_presentation =
        build_pane_placement_presentation(&client, &render_snapshot).expect("drag preview");
    assert_eq!(
        dragged_presentation.source_message.detail_text.as_deref(),
        Some("Release to place · Esc to cancel")
    );
    client.placement_state.placement_drag = None;

    client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("placement mode")
        .placement_target = Some(TiledPlacementTarget::Split {
        destination_tab_id: source_tab_id,
        anchor: PanePlacementAnchor::Tab,
        direction: Direction::Right,
    });
    let tab_presentation =
        build_pane_placement_presentation(&client, &render_snapshot).expect("whole-tab preview");
    assert_eq!(
        tab_presentation.target_pane_ids,
        vec![first_target_pane_id, second_target_pane_id]
    );
    assert_eq!(
        tab_presentation
            .target_message
            .as_ref()
            .map(|pane_message| pane_message.full_text.as_str()),
        Some("Other panes will make room to the right")
    );

    let mut cross_tab_snapshot = crate::tests::build_test_placement_snapshot(
        source_pane_id,
        source_tab_id,
        destination_tab_id,
    );
    let cross_tab_target_pane_id = crate::tests::get_destination_pane_id(&cross_tab_snapshot);
    get_source_tab_snapshot_mut(&mut cross_tab_snapshot)
        .tab_snapshot
        .tab_name = "work".to_string();
    crate::tests::hold_placement_snapshot(&mut client, cross_tab_snapshot);
    let placement_mode = client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("placement mode");
    placement_mode.destination_tab_id = destination_tab_id;
    placement_mode.placement_target = Some(TiledPlacementTarget::Swap {
        target_pane_id: cross_tab_target_pane_id,
    });
    let cross_tab_presentation =
        build_pane_placement_presentation(&client, &render_snapshot).expect("cross-tab preview");
    assert_eq!(
        cross_tab_presentation.target_pane_ids,
        vec![cross_tab_target_pane_id]
    );
    assert_eq!(
        cross_tab_presentation.source_message.detail_text.as_deref(),
        Some("Other pane takes the old place in work")
    );
}

#[test]
fn placement_presentation_names_the_sessions_refusal_of_the_selected_target() {
    let mut client = build_test_client();
    let mut render_snapshot =
        build_render_snapshot(&build_received_painted_frame(build_test_painted_frame()));
    let source_tab_id = render_snapshot.client_snapshot.active_tab_id;
    let destination_tab_id = TabId::new();
    let source_pane_id = PaneId::new();
    render_snapshot.session_snapshot.tabs_metadata = vec![
        TabMetadata {
            tab_id: source_tab_id,
            tab_name: "main".to_string(),
            tab_index: 0,
            is_active: true,
        },
        TabMetadata {
            tab_id: destination_tab_id,
            tab_name: "logs".to_string(),
            tab_index: 1,
            is_active: false,
        },
    ];
    client.set_frame_view(
        source_tab_id,
        Some(source_pane_id),
        Some(source_pane_id),
        vec![source_tab_id, destination_tab_id],
    );
    let placement_snapshot = crate::tests::build_test_placement_snapshot(
        source_pane_id,
        source_tab_id,
        destination_tab_id,
    );
    let target_pane_id = crate::tests::get_destination_pane_id(&placement_snapshot);
    crate::tests::hold_placement_snapshot(&mut client, placement_snapshot);
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id,
        placement_direction: Direction::Right,
        placement_target: Some(TiledPlacementTarget::Split {
            destination_tab_id,
            anchor: PanePlacementAnchor::Pane(target_pane_id),
            direction: Direction::Right,
        }),
        pending_placement_command_id: None,
    });
    let placement_preview = Arc::clone(
        client
            .find_held_placement_preview()
            .expect("the preview is held"),
    );
    let placement_target = client
        .get_placement_target()
        .cloned()
        .expect("a target is selected");
    let mut build_refused_presentation = |placement_refusal: PlacementPreviewError| {
        client.placement_state.placement_target_outcome = Some(PlacementTargetOutcome {
            placement_preview: Arc::clone(&placement_preview),
            placement_target: placement_target.clone(),
            placed_preview: Err(placement_refusal),
        });
        build_pane_placement_presentation(&client, &render_snapshot)
            .expect("the pane placement has a visible preview")
    };
    let build_expected_presentation =
        |full_text: &str, compact_text: &'static str| PanePlacementPresentation {
            source_pane_id,
            target_pane_ids: vec![target_pane_id],
            source_message: PanePlacementMessage {
                full_text: "Moving pane cannot go there".to_string(),
                compact_text: "Moving pane",
                detail_text: Some("Choose another place · Esc to cancel".to_string()),
            },
            target_message: Some(PanePlacementMessage {
                full_text: full_text.to_string(),
                compact_text,
                detail_text: Some("Choose another place · Esc to cancel".to_string()),
            }),
        };

    assert_eq!(
        build_refused_presentation(PlacementPreviewError::NoRoom {
            tab_id: destination_tab_id
        }),
        build_expected_presentation("No room for this pane in logs", "No room")
    );
    assert_eq!(
        build_refused_presentation(PlacementPreviewError::NoRoom {
            tab_id: source_tab_id
        }),
        build_expected_presentation("No room for the panes left in main", "No room")
    );
    assert_eq!(
        build_refused_presentation(PlacementPreviewError::NoDestinationTabSize),
        build_expected_presentation("No room to show logs", "No room")
    );
    assert_eq!(
        build_refused_presentation(PlacementPreviewError::UnpreviewedTarget),
        build_expected_presentation("This place is not available", "Not available")
    );
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
    let terminal_buffer_before_refresh = screen.paint_driver.backend.buffer().clone();

    client.set_viewport_size(Size {
        column_count: 100,
        row_count: 30,
    });
    client.set_link_status(LinkStatus::Redialing(Reconnecting {
        attempt: 1,
        retry_in_seconds: Some(1),
    }));
    screen.refresh(&mut client, Some(active_tab_id));

    assert_eq!(
        screen.paint_driver.backend.buffer(),
        &terminal_buffer_before_refresh
    );
    assert_eq!(screen.committed_regions.viewport_size, TEST_VIEWPORT_SIZE);
}

/// The hint bar of what the screen last drew: the bottom row, trailing blanks
/// removed.
fn get_hint_row(screen: &Screen<TestBackend>) -> String {
    let screen_buffer = screen.paint_driver.backend.buffer();
    let column_count = screen_buffer.area.width as usize;
    screen_buffer
        .content()
        .chunks(column_count)
        .last()
        .expect("a drawn frame has at least one row")
        .iter()
        .map(|buffer_cell| buffer_cell.symbol())
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// The first chord of a multi-chord binding the built-in normal mode carries:
/// one press that opens a sequence and sends nothing.
fn get_sequence_opener(client: &Client) -> KeyChord {
    client
        .keymap_catalog
        .build_frame_hints(client.get_lock_mode(), false, false)
        .hint_bindings
        .iter()
        .find(|hint_binding| hint_binding.key_sequence.list_chords().len() > 1)
        .expect("the built-in normal mode binds at least one multi-chord sequence")
        .key_sequence
        .list_chords()[0]
}

/// The pointer moved to `screen_point` with no button held.
fn build_mouse_motion(screen_point: Point) -> MouseInput {
    MouseInput {
        mouse_kind: MouseKind::Motion,
        position: screen_point,
        modifier_flags: BindingModifierFlags::NONE,
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
    let normal_hint_row = get_hint_row(&screen);

    screen
        .draw_painted_frame(
            &mut client,
            Box::new(build_test_painted_frame_with_lock_mode(LockMode::Locked)),
        )
        .expect("paint");
    let locked_hint_row = get_hint_row(&screen);

    assert_eq!(
        locked_hint_row,
        " Ctrl +  l  Unlock  g  Mouse Select  q  Quit"
    );
    assert_ne!(normal_hint_row, locked_hint_row);
}

#[test]
fn a_frame_moves_the_viewer_to_the_mode_it_reports() {
    let mut client = build_test_client();
    assert_eq!(client.get_lock_mode(), LockMode::Normal);

    client.apply_render_snapshot(&build_render_snapshot(&build_received_painted_frame(
        build_test_painted_frame_with_lock_mode(LockMode::Locked),
    )));

    assert_eq!(client.get_lock_mode(), LockMode::Locked);
}

#[test]
fn a_prefix_key_typed_after_a_frame_in_one_pass_still_draws_its_breadcrumb() {
    // One drained batch carries a frame and a keypress together. The frame is
    // drawn first, the key opens a sequence after it, and the end of the pass
    // draws again.
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let painted_frame = build_test_painted_frame_with_lock_mode(LockMode::Normal);
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;

    screen
        .draw_painted_frame(&mut client, Box::new(painted_frame))
        .expect("paint");
    let drawn_hint_row = get_hint_row(&screen);

    let sequence_opener_chord = get_sequence_opener(&client);
    assert_eq!(
        client.resolve_key(sequence_opener_chord, Instant::now()),
        KeyOutcome::Pending
    );
    screen.refresh(&mut client, Some(active_tab_id));

    // The breadcrumb view opens with the chord that was pressed and ends with
    // the arrow that separates it from the chords continuing it.
    let breadcrumb_hint_row = get_hint_row(&screen);
    assert_ne!(breadcrumb_hint_row, drawn_hint_row);
    assert!(
        breadcrumb_hint_row.contains(" ▶ "),
        "the bar draws the open sequence: {breadcrumb_hint_row}"
    );
}

#[test]
fn a_pointer_moved_after_a_frame_in_one_pass_still_draws_the_new_hover() {
    // The same batch order with a mouse move: the pane under the pointer is the
    // viewer's own, and the frame drawn before the move does not carry it.
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let painted_frame = build_test_painted_frame_with_lock_mode(LockMode::Normal);
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[
        build_plain_mouse_pane(first_pane_id),
        build_plain_mouse_pane(second_pane_id),
    ]);
    let mut pending_mouse_actions = Vec::new();

    screen
        .draw_painted_frame(&mut client, Box::new(painted_frame))
        .expect("paint");
    let painted_render_snapshot = screen.last_snapshot.clone().expect("the frame painted");
    let hovered_pane_id_before =
        ViewerPaint::from_client(&client, active_tab_id, &painted_render_snapshot)
            .chrome
            .hovered_pane_id;

    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_motion(get_content_cell(&mouse_frame, 1)),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    let hovered_pane_id_after =
        ViewerPaint::from_client(&client, active_tab_id, &painted_render_snapshot)
            .chrome
            .hovered_pane_id;
    screen.refresh(&mut client, Some(active_tab_id));

    assert_eq!(hovered_pane_id_before, None);
    assert_eq!(hovered_pane_id_after, Some(second_pane_id));
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
fn each_painted_mouse_frame_carries_the_viewer_chrome_it_was_painted_with() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let painted_frame = build_test_painted_frame_with_lock_mode(LockMode::Normal);
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;
    let second_pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[
        build_plain_mouse_pane(PaneId::new()),
        build_plain_mouse_pane(second_pane_id),
    ]);
    let mut pending_mouse_actions = Vec::new();

    let drawn_mouse_frame = screen
        .draw_painted_frame(&mut client, Box::new(painted_frame))
        .expect("paint");
    assert_eq!(
        Some(drawn_mouse_frame.viewer_chrome),
        screen
            .shown_viewer_paint
            .as_ref()
            .map(|viewer_paint| viewer_paint.chrome)
    );
    assert_eq!(
        drawn_mouse_frame.viewer_chrome.active_input_mode,
        Some(LockMode::Normal)
    );

    handle_mouse_event(
        &mut client,
        &mouse_frame,
        build_mouse_motion(get_content_cell(&mouse_frame, 1)),
        Instant::now(),
        &mut pending_mouse_actions,
    );
    let refreshed_mouse_frame = screen
        .refresh(&mut client, Some(active_tab_id))
        .expect("the hover repaints");

    assert_eq!(
        Some(refreshed_mouse_frame.viewer_chrome),
        screen
            .shown_viewer_paint
            .as_ref()
            .map(|viewer_paint| viewer_paint.chrome)
    );
    assert_eq!(
        refreshed_mouse_frame.viewer_chrome.hovered_pane_id,
        Some(second_pane_id)
    );
}

#[test]
fn a_pass_without_viewer_changes_skips_terminal_draw() {
    let mut client = build_test_client();
    let draw_count = Arc::new(AtomicUsize::new(0));
    let mut screen = Screen::from_backend_and_viewport_size(
        FailingBackend::from_terminal_size_with_draw_count(
            TEST_VIEWPORT_SIZE,
            Arc::clone(&draw_count),
        ),
        TEST_VIEWPORT_SIZE,
    );
    let painted_frame = build_test_painted_frame_with_lock_mode(LockMode::Normal);
    let active_tab_id = painted_frame.client_snapshot.active_tab_id;

    screen
        .draw_painted_frame(&mut client, Box::new(painted_frame))
        .expect("paint");
    assert_eq!(draw_count.load(Ordering::Relaxed), 1);
    let shown_viewer_paint = screen.shown_viewer_paint.clone();

    assert_eq!(screen.refresh(&mut client, Some(active_tab_id)), None);

    assert_eq!(draw_count.load(Ordering::Relaxed), 1);
    assert_eq!(screen.shown_viewer_paint, shown_viewer_paint);
}

#[test]
fn attachment_skips_a_redraw_after_paste_when_viewer_paint_is_unchanged() {
    let draw_count = Arc::new(AtomicUsize::new(0));
    let mut screen = Screen::from_backend_and_viewport_size(
        FailingBackend::from_terminal_size_with_draw_count(
            TEST_VIEWPORT_SIZE,
            Arc::clone(&draw_count),
        ),
        TEST_VIEWPORT_SIZE,
    );
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    let producer_sender = incoming_sender.clone();
    let producer_draw_count = Arc::clone(&draw_count);
    let producer_thread = thread::spawn(move || {
        producer_sender
            .send(Incoming::Painted {
                connection_index: INITIAL_CONNECTION_INDEX,
                received_painted_frame: Box::new(build_received_painted_frame(
                    build_test_painted_frame(),
                )),
            })
            .expect("the attachment loop accepts the initial frame");
        assert!(
            wait_for_draw_count(&producer_draw_count, 1),
            "the initial frame reaches the terminal"
        );
        assert_eq!(
            request_receiver
                .recv_timeout(Duration::from_secs(5))
                .map(|paint_report| paint_report.request_kind),
            Ok(IpcRequestKind::PaintedUpdate { update_sequence: 1 }),
            "the pass that paints the frame reports it painted"
        );
        producer_sender
            .send(build_input_incoming(
                RuntimeEvent::HostPaste {
                    client_id,
                    pasted_text: String::from("viewer input"),
                },
                Instant::now(),
            ))
            .expect("the attachment loop accepts the paste");
        let paste_request = request_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("the paste reaches the session");
        assert_eq!(
            paste_request.request_kind,
            IpcRequestKind::Paste {
                pasted_text: String::from("viewer input"),
            }
        );
        producer_sender
            .send(build_input_incoming(RuntimeEvent::Quit, Instant::now()))
            .expect("the attachment loop accepts the terminal end");
    });
    let attachment_ending = run_attachment(
        &build_local_home(),
        SessionId::new(),
        client_id,
        ConnectionToken::generate(),
        None,
        &mut client,
        &mut screen,
        &mut uplink,
        TEXT_ONLY_ATTACH_SETTINGS,
        &mut terminal::CellSizeQuery::from_current_measurement(None, false, false),
        &mut Vec::new(),
        incoming_sender,
        incoming_receiver,
    );
    producer_thread.join().expect("the producer finished");

    assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
    assert_eq!(draw_count.load(Ordering::Relaxed), 1);
}

#[test]
fn a_release_owed_outside_a_mouse_event_leaves_in_the_next_round() {
    let pane_id = PaneId::new();
    let mut client = build_client_capturing_left_press(pane_id);
    // Placement mode opening from a key ends the capture this way.
    client.end_held_mouse_gestures();

    // No frame is painted in the pass.
    let (attachment_ending, first_request_kind) = run_attachment_until_first_request(
        &mut client,
        &mut build_test_screen(),
        build_session_event_incoming(SessionEvent::Pong),
    );

    assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
    assert_eq!(
        first_request_kind,
        Ok(IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
            pane_id,
            mouse_kind: MouseKind::Release(MouseButton::Left),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point { column: 1, row: 1 },
        }])),
        "the program that saw the press gets its release in the first pass's round"
    );
}

/// Run an attachment for `client` with `screen`, while a producer sends
/// `first_incoming`, reads the first request the loop writes, and then
/// ends the terminal. Returns how the attachment ended and the kind of that
/// first request, or the wait that ran out after 5 seconds.
fn run_attachment_until_first_request(
    client: &mut Client,
    screen: &mut Screen<TestBackend>,
    first_incoming: Incoming,
) -> (
    AttachmentEnding,
    Result<IpcRequestKind, mpsc::RecvTimeoutError>,
) {
    let client_id = client.get_client_id();
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    let producer_sender = incoming_sender.clone();
    let producer_thread = thread::spawn(move || {
        producer_sender
            .send(first_incoming)
            .expect("the attachment loop accepts the frame");
        let first_request = request_receiver.recv_timeout(Duration::from_secs(5));
        producer_sender
            .send(build_input_incoming(RuntimeEvent::Quit, Instant::now()))
            .expect("the attachment loop accepts the terminal end");
        first_request.map(|first_request| first_request.request_kind)
    });
    let attachment_ending = run_attachment(
        &build_local_home(),
        SessionId::new(),
        client_id,
        ConnectionToken::generate(),
        None,
        client,
        screen,
        &mut uplink,
        TEXT_ONLY_ATTACH_SETTINGS,
        &mut terminal::CellSizeQuery::from_current_measurement(None, false, false),
        &mut Vec::new(),
        incoming_sender,
        incoming_receiver,
    );
    let first_request_kind = producer_thread.join().expect("the producer finished");
    (attachment_ending, first_request_kind)
}

/// A client whose left press in `pane_id`, a pane that reports drags, is
/// still captured at pane cell `(1, 1)`.
fn build_client_capturing_left_press(pane_id: PaneId) -> Client {
    let mouse_frame = build_mouse_frame(&[MousePane {
        mouse_tracking: MouseTracking::ButtonMotion,
        ..build_plain_mouse_pane(pane_id)
    }]);
    let mut client = build_test_client();
    client.handle_mouse(
        build_mouse_press(get_content_cell(&mouse_frame, 0)),
        &mouse_frame,
        Instant::now(),
    );
    client
}

#[test]
fn a_painted_frame_that_no_longer_draws_a_captured_pane_sends_its_release() {
    let pane_id = PaneId::new();
    let mut client = build_client_capturing_left_press(pane_id);

    // The painted frame draws only panes of its own, not `pane_id`.
    let (attachment_ending, first_request_kind) = run_attachment_until_first_request(
        &mut client,
        &mut build_test_screen(),
        Incoming::Painted {
            connection_index: INITIAL_CONNECTION_INDEX,
            received_painted_frame: Box::new(build_received_painted_frame(
                build_test_painted_frame(),
            )),
        },
    );

    assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
    assert_eq!(
        first_request_kind,
        Ok(IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
            pane_id,
            mouse_kind: MouseKind::Release(MouseButton::Left),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point { column: 1, row: 1 },
        }])),
        "the pass that paints the frame sends the release, with no mouse event"
    );
}

#[test]
fn a_frame_committed_at_the_end_of_a_pass_sends_the_release_it_ended_in_that_pass() {
    let pane_id = PaneId::new();
    let mut client = build_client_capturing_left_press(pane_id);
    // A frame that waits to be committed. The end of the first pass commits it.
    let mut screen = build_test_screen();
    screen.pending_snapshot = Some(build_render_snapshot(&build_received_painted_frame(
        build_test_painted_frame(),
    )));

    let (attachment_ending, first_request_kind) = run_attachment_until_first_request(
        &mut client,
        &mut screen,
        build_session_event_incoming(SessionEvent::Pong),
    );

    assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
    assert_eq!(
        first_request_kind,
        Ok(IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
            pane_id,
            mouse_kind: MouseKind::Release(MouseButton::Left),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point { column: 1, row: 1 },
        }])),
        "the release joins the round of the pass whose end committed the frame"
    );
}

#[test]
fn a_failed_shift_mouse_request_ends_the_attachment() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: pane_id,
        source_tab_id: Some(tab_id),
        destination_tab_id: tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
    assert!(client.is_placement_mode_active());

    let mut screen = build_test_screen();
    let (request_sender, _request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    incoming_sender
        .send(build_input_incoming(
            RuntimeEvent::HostPaste {
                client_id,
                pasted_text: String::from("discarded while placing"),
            },
            Instant::now(),
        ))
        .expect("the attachment loop accepts the input");
    let mut terminal_mode_bytes: &mut [u8] = &mut [];

    let attachment_ending = run_attachment(
        &build_local_home(),
        SessionId::new(),
        client_id,
        ConnectionToken::generate(),
        None,
        &mut client,
        &mut screen,
        &mut uplink,
        TEXT_ONLY_ATTACH_SETTINGS,
        &mut terminal::CellSizeQuery::from_current_measurement(None, false, false),
        &mut terminal_mode_bytes,
        incoming_sender,
        incoming_receiver,
    );

    assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
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
    let locked_render_snapshot = build_render_snapshot(&build_received_painted_frame(
        build_test_painted_frame_with_lock_mode(LockMode::Locked),
    ));
    let active_tab_id = locked_render_snapshot.client_snapshot.active_tab_id;
    let paint_before_lock =
        ViewerPaint::from_client(&client, active_tab_id, &locked_render_snapshot);

    client.apply_render_snapshot(&locked_render_snapshot);

    let paint_after_lock =
        ViewerPaint::from_client(&client, active_tab_id, &locked_render_snapshot);
    assert_eq!(paint_before_lock.lock_mode, LockMode::Normal);
    assert_eq!(paint_after_lock.lock_mode, LockMode::Locked);
    assert_ne!(paint_after_lock, paint_before_lock);
}

#[test]
fn opening_a_key_sequence_moves_what_the_viewer_paints() {
    // An opened sequence reaches no session, and no frame comes back.
    // `ViewerPaint` carries the sequence, and the hint bar draws it as a
    // breadcrumb.
    let mut client = build_test_client();
    let render_snapshot = build_render_snapshot(&build_received_painted_frame(
        build_test_painted_frame_with_lock_mode(LockMode::Normal),
    ));
    let active_tab_id = render_snapshot.client_snapshot.active_tab_id;
    let sequence_opener_chord = get_sequence_opener(&client);
    let paint_before_key_sequence =
        ViewerPaint::from_client(&client, active_tab_id, &render_snapshot);

    assert_eq!(
        client.resolve_key(sequence_opener_chord, Instant::now()),
        KeyOutcome::Pending
    );

    let paint_after_key_sequence =
        ViewerPaint::from_client(&client, active_tab_id, &render_snapshot);
    assert_eq!(paint_before_key_sequence.pending_key_sequence, None);
    assert_eq!(
        paint_after_key_sequence.pending_key_sequence,
        Some(KeySequence::from(sequence_opener_chord))
    );
    assert_ne!(paint_after_key_sequence, paint_before_key_sequence);
}

#[test]
fn dialing_again_moves_what_the_viewer_paints() {
    // No frame arrives while the link is down. A moved `ViewerPaint` fires the
    // repaint that draws the `RECONNECTING` tag.
    let mut client = build_test_client();
    let render_snapshot = build_render_snapshot(&build_received_painted_frame(
        build_test_painted_frame_with_lock_mode(LockMode::Normal),
    ));
    let active_tab_id = render_snapshot.client_snapshot.active_tab_id;
    let paint_before_reconnect = ViewerPaint::from_client(&client, active_tab_id, &render_snapshot);

    let reconnecting = Reconnecting {
        attempt: 1,
        retry_in_seconds: Some(5),
    };
    client.set_link_status(LinkStatus::Redialing(reconnecting));

    let paint_after_reconnect = ViewerPaint::from_client(&client, active_tab_id, &render_snapshot);
    assert_eq!(paint_before_reconnect.chrome.reconnecting, None);
    assert_eq!(
        paint_after_reconnect.chrome.reconnecting,
        Some(reconnecting)
    );
    assert_ne!(paint_after_reconnect, paint_before_reconnect);
}

/// A test viewer that drew one frame and lost its link, with the loop's channel
/// it reads and the cell-size query it answers.
struct TestViewerWithoutLink {
    client: Client,
    screen: Screen<TestBackend>,
    /// The active tab of the one frame `screen` drew.
    active_tab_id: TabId,
    cell_size_query: terminal::CellSizeQuery,
    incoming_sender: mpsc::SyncSender<Incoming>,
    incoming_receiver: mpsc::Receiver<Incoming>,
}

impl TestViewerWithoutLink {
    /// A [`build_test_client`] viewer that drew one [`build_test_painted_frame`],
    /// with no cell-size reply pending.
    fn new() -> Self {
        let mut client = build_test_client();
        let mut screen = build_test_screen();
        let painted_frame = build_test_painted_frame();
        let active_tab_id = painted_frame.client_snapshot.active_tab_id;
        screen
            .draw_painted_frame(&mut client, Box::new(painted_frame))
            .expect("paint");
        let (incoming_sender, incoming_receiver) = build_incoming_channel();
        TestViewerWithoutLink {
            client,
            screen,
            active_tab_id,
            cell_size_query: terminal::CellSizeQuery::from_current_measurement(None, false, false),
            incoming_sender,
            incoming_receiver,
        }
    }

    /// Put `incoming_event` on the loop's channel.
    fn queue_incoming_event(&self, incoming_event: Incoming) {
        self.incoming_sender
            .send(incoming_event)
            .expect("the loop's channel takes it");
    }

    /// `key_chord` pressed on this viewer's terminal.
    fn build_key_press_event(&self, key_chord: KeyChord) -> Incoming {
        build_input_incoming(
            RuntimeEvent::KeyInput {
                client_id: self.client.get_client_id(),
                key_input: crate::tests::build_key_input_for_chord(key_chord),
            },
            Instant::now(),
        )
    }

    /// Run [`run_without_link`] for `link_recovery`, reading `leftover_events`
    /// first, with `attempt_rejoin` as every attempt and an uplink whose queue
    /// nobody reads.
    fn run_without_link(
        &mut self,
        link_recovery: LinkRecovery,
        leftover_events: Vec<Incoming>,
        attempt_rejoin: Arc<dyn Fn() -> RejoinAnswer + Send + Sync>,
    ) -> Result<(JoinedSession, Vec<HeldInput>), AttachmentEnding> {
        let (request_sender, _request_receiver) = mpsc::channel();
        let uplink = build_test_uplink(request_sender);
        run_without_link(
            link_recovery,
            leftover_events,
            &attempt_rejoin,
            &mut self.client,
            &mut self.screen,
            Some(self.active_tab_id),
            &uplink,
            &mut self.cell_size_query,
            &self.incoming_sender,
            &self.incoming_receiver,
        )
    }
}

/// `<C-q>`, the chord the built-in keymap binds to `core:quit` in normal and
/// locked mode.
fn build_quit_chord() -> KeyChord {
    KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('q'))
}

/// `character` typed with no modifier.
fn build_plain_chord(character: char) -> KeyChord {
    KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char(character))
}

/// `<C-l>`, the chord the built-in keymap binds to `core:lock` in normal mode
/// and to `core:unlock` in locked mode.
fn build_lock_chord() -> KeyChord {
    KeybindingsConfig::RESERVED_UNLOCK
}

/// `<Tab>`, the key the built-in keymap binds to `core:next-tab` in normal
/// mode and passes to the pane in locked mode.
fn build_tab_chord() -> KeyChord {
    KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Tab))
}

/// The `SetLockMode` command a lock binding sends: `is_locked` for
/// `is_locked`, naming no client.
fn build_expected_lock_command(is_locked: bool) -> Command {
    Command::SetLockMode(LockModeArgs {
        is_locked,
        client_id: None,
    })
}

/// An attempt that adds one to the returned count each time it runs and
/// answers unreachable.
fn build_counted_attempt() -> (
    Arc<dyn Fn() -> RejoinAnswer + Send + Sync>,
    Arc<AtomicUsize>,
) {
    let attempt_count = Arc::new(AtomicUsize::new(0));
    let counted_attempt_count = Arc::clone(&attempt_count);
    let attempt_rejoin: Arc<dyn Fn() -> RejoinAnswer + Send + Sync> = Arc::new(move || {
        counted_attempt_count.fetch_add(1, Ordering::SeqCst);
        RejoinAnswer::Unreachable(CliError::IpcUnavailable {
            detail: String::from("connection refused"),
        })
    });
    (attempt_rejoin, attempt_count)
}

/// How long an attempt from [`build_blocked_attempt`] waits for its release: 10
/// seconds.
const BLOCKED_ATTEMPT_WAIT_DURATION: Duration = Duration::from_secs(10);

/// An attempt that reports its start on the returned receiver, then waits
/// until the returned sender is dropped or [`BLOCKED_ATTEMPT_WAIT_DURATION`]
/// passes, and answers unreachable.
fn build_blocked_attempt() -> (
    Arc<dyn Fn() -> RejoinAnswer + Send + Sync>,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
) {
    let (attempt_start_sender, attempt_start_receiver) = mpsc::channel::<()>();
    let (attempt_release_sender, attempt_release_receiver) = mpsc::channel::<()>();
    let attempt_release_receiver = Mutex::new(attempt_release_receiver);
    let attempt_rejoin: Arc<dyn Fn() -> RejoinAnswer + Send + Sync> = Arc::new(move || {
        let _ = attempt_start_sender.send(());
        let _ = attempt_release_receiver
            .lock()
            .expect("the release outlives every panic")
            .recv_timeout(BLOCKED_ATTEMPT_WAIT_DURATION);
        RejoinAnswer::Unreachable(CliError::IpcUnavailable {
            detail: String::from("the test released the attempt"),
        })
    });
    (
        attempt_rejoin,
        attempt_start_receiver,
        attempt_release_sender,
    )
}

/// A session that `client_id` joined on a fresh local connection, and the
/// session end of that connection.
fn build_joined_session(client_id: ClientId) -> (JoinedSession, Connection) {
    let (session_connection, reader, writer) = build_test_event_stream();
    let joined_session = JoinedSession {
        reader,
        writer,
        client_id,
        session_id: SessionId::new(),
        connection_token: ConnectionToken::from_secret(NEW_CONNECTION_TOKEN),
        resume_token: None,
    };
    (joined_session, session_connection)
}

/// An attempt answer that joined `client_id` to a session on a fresh local
/// connection, and the session end of that connection.
fn build_joined_answer(client_id: ClientId) -> (Incoming, Connection) {
    let (joined_session, session_connection) = build_joined_session(client_id);
    let joined_answer = Incoming::RejoinAnswer(Box::new(RejoinAnswer::Joined(
        JoinedSessionGuard::from_joined_session(joined_session),
    )));
    (joined_answer, session_connection)
}

/// Read the `Leaving { is_returning: true }` request that a join the loop did
/// not take sends, then the close of its connection, from `session_connection`.
fn assert_joined_connection_leaves_then_closes(session_connection: &mut Connection) {
    assert_eq!(
        session_connection
            .recv::<IpcRequest>()
            .expect("the joined connection says it leaves"),
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Leaving { is_returning: true },
        }
    );
    let Err(IpcError::Disconnected) = session_connection.recv::<IpcRequest>() else {
        panic!("the joined connection closes after it says it leaves");
    };
}

/// The `RECONNECTING` tag in the tabline the screen last drew, from
/// `RECONNECTING` through its closing parenthesis, or `None` when the tabline
/// carries no tag.
fn find_drawn_reconnecting_tag(screen: &Screen<TestBackend>) -> Option<String> {
    let screen_buffer = screen.paint_driver.backend.buffer();
    let column_count = screen_buffer.area.width as usize;
    let tabline_text = screen_buffer
        .content()
        .chunks(column_count)
        .next()
        .expect("a drawn frame has at least one row")
        .iter()
        .map(|buffer_cell| buffer_cell.symbol())
        .collect::<String>();
    let tag_start_index = tabline_text.find("RECONNECTING")?;
    let tag_byte_count = tabline_text[tag_start_index..].find(')')? + 1;
    Some(tabline_text[tag_start_index..tag_start_index + tag_byte_count].to_string())
}

/// Bind `key_sequence` to `core:quit` in `client`'s normal mode, on top of the
/// built-in keymap.
fn bind_quit_in_normal_mode(client: &mut Client, key_sequence: KeySequence) {
    bind_core_actions_in_normal_mode(client, &[(key_sequence, "quit")], None);
}

/// Bind each `(key sequence, core action name)` of `key_bindings` in
/// `client`'s normal mode, over the built-in keymap, with a chord timeout of
/// `chord_timeout_ms` when it is given and the built-in one otherwise.
fn bind_core_actions_in_normal_mode(
    client: &mut Client,
    key_bindings: &[(KeySequence, &str)],
    chord_timeout_ms: Option<u32>,
) {
    let bound_action_by_key_sequence = key_bindings
        .iter()
        .map(|(key_sequence, action_name)| {
            (key_sequence.clone(), build_core_bound_action(action_name))
        })
        .collect();
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: BTreeSet::new(),
        },
    );
    client.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            mode_bindings_by_name: Some(mode_bindings_by_name),
            chord_timeout_ms,
            ..PartialKeybindingsConfig::default()
        }),
    );
}

/// The schedule of a link that dropped one redial wait ago: its first dial is
/// due now.
fn build_due_redial_schedule() -> RedialSchedule {
    RedialSchedule::from_link_drop_at(compute_instant_before_now(FIRST_REDIAL_WAIT_DURATION))
}

#[test]
fn the_redial_countdown_reads_whole_seconds_rounded_up_from_the_next_dial() {
    let link_dropped_at = Instant::now();
    let mut redial_schedule = RedialSchedule::from_link_drop_at(link_dropped_at);

    assert_eq!(
        redial_schedule.build_reconnecting(link_dropped_at),
        Reconnecting {
            attempt: 1,
            retry_in_seconds: Some(1),
        }
    );
    assert_eq!(
        redial_schedule.compute_next_wakeup(link_dropped_at),
        Some(Duration::from_secs(1))
    );
    let before_first_dial = link_dropped_at + Duration::from_millis(300);
    assert_eq!(
        redial_schedule.build_reconnecting(before_first_dial),
        Reconnecting {
            attempt: 1,
            retry_in_seconds: Some(1),
        }
    );
    assert_eq!(
        redial_schedule.compute_next_wakeup(before_first_dial),
        Some(Duration::from_millis(700))
    );
    assert!(!redial_schedule.start_due_dial(before_first_dial));

    let first_dial_at = link_dropped_at + FIRST_REDIAL_WAIT_DURATION;
    assert!(redial_schedule.start_due_dial(first_dial_at));
    assert_eq!(
        redial_schedule.build_reconnecting(first_dial_at),
        Reconnecting {
            attempt: 1,
            retry_in_seconds: None,
        }
    );
    assert_eq!(redial_schedule.compute_next_wakeup(first_dial_at), None);
    assert!(
        !redial_schedule.start_due_dial(first_dial_at + Duration::from_secs(5)),
        "a running dial is never started again"
    );

    let first_dial_failed_at = link_dropped_at + Duration::from_millis(1500);
    assert!(redial_schedule.schedule_next_dial(first_dial_failed_at));
    assert_eq!(
        redial_schedule.build_reconnecting(first_dial_failed_at),
        Reconnecting {
            attempt: 2,
            retry_in_seconds: Some(2),
        }
    );
    assert_eq!(
        redial_schedule.compute_next_wakeup(first_dial_failed_at),
        Some(Duration::from_secs(1))
    );
    let during_second_pause = first_dial_failed_at + Duration::from_millis(1300);
    assert_eq!(
        redial_schedule.build_reconnecting(during_second_pause),
        Reconnecting {
            attempt: 2,
            retry_in_seconds: Some(1),
        }
    );
    assert_eq!(
        redial_schedule.compute_next_wakeup(during_second_pause),
        Some(Duration::from_millis(700))
    );
}

#[test]
fn redials_go_out_on_the_widening_schedule_and_stop_inside_the_window() {
    let link_dropped_at = Instant::now();
    let mut redial_schedule = RedialSchedule::from_link_drop_at(link_dropped_at);
    let mut dial_offset_seconds = Vec::new();

    loop {
        let next_dial_at = redial_schedule
            .next_dial_at
            .expect("a waiting schedule names its next dial");
        assert!(redial_schedule.start_due_dial(next_dial_at));
        dial_offset_seconds.push((next_dial_at - link_dropped_at).as_secs());
        if !redial_schedule.schedule_next_dial(next_dial_at) {
            break;
        }
    }

    assert_eq!(
        dial_offset_seconds,
        vec![1, 3, 7, 15, 23, 31, 39, 47, 55, 63, 71, 79, 87, 95, 103, 111, 119]
    );
    assert_eq!(redial_schedule.attempt, 17);
    assert_eq!(
        redial_schedule.next_dial_at, None,
        "a pause past the window is not taken"
    );
}

#[test]
fn a_restart_hold_takes_inputs_up_to_its_count_and_paste_caps() {
    let mut input_hold = InputHold::default();
    for _ in 0..MAX_HELD_INPUT_COUNT {
        assert!(input_hold.hold_input(HeldInput::Keyboard(
            crate::tests::build_key_input_for_chord(build_plain_chord('a'))
        )));
    }
    assert!(!input_hold.hold_input(HeldInput::Keyboard(
        crate::tests::build_key_input_for_chord(build_plain_chord('b'))
    )));
    assert_eq!(input_hold.held_inputs.len(), MAX_HELD_INPUT_COUNT);

    let mut paste_hold = InputHold::default();
    let frame_of_text = "x".repeat(MAX_FRAME_BYTE_COUNT as usize);
    assert!(paste_hold.hold_input(HeldInput::Paste(frame_of_text.clone())));
    assert!(!paste_hold.hold_input(HeldInput::Paste(String::from("y"))));
    assert_eq!(
        paste_hold.held_inputs,
        vec![HeldInput::Paste(frame_of_text)]
    );
    assert_eq!(
        paste_hold.held_paste_byte_count,
        MAX_FRAME_BYTE_COUNT as usize
    );
}

#[test]
fn an_overflowing_restart_hold_drops_everything_typed_during_that_restart() {
    let mut client = build_test_client();
    let mut link_recovery = LinkRecovery::RestartRejoin {
        input_hold: Some(InputHold::default()),
    };
    for _ in 0..=MAX_HELD_INPUT_COUNT {
        link_recovery.hold_input(
            &mut client,
            HeldInput::Keyboard(crate::tests::build_key_input_for_chord(build_plain_chord(
                'a',
            ))),
        );
    }
    link_recovery.hold_input(&mut client, HeldInput::Paste(String::from("after")));

    let LinkRecovery::RestartRejoin { input_hold } = link_recovery else {
        panic!("holding never changes the recovery");
    };
    assert_eq!(input_hold.map(|input_hold| input_hold.held_inputs), None);
}

#[test]
fn a_restart_during_pane_placement_holds_nothing() {
    let mut client = build_test_client();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    let mut restart_recovery = LinkRecovery::RestartRejoin {
        input_hold: Some(InputHold::default()),
    };
    restart_recovery.hold_input(&mut client, HeldInput::Paste(String::from("typed")));
    let LinkRecovery::RestartRejoin { input_hold } = restart_recovery else {
        panic!("holding never changes the recovery");
    };
    assert_eq!(
        input_hold.map(|input_hold| input_hold.held_inputs),
        Some(Vec::new())
    );
}

/// One entry per viewer-local mode a client can be in with no frame: the move
/// chooser, floating pane move mode of a floating pane that no frame names, and
/// dock/undock mode. Each entry turns that mode on in the client it is given.
fn list_local_mode_entries() -> [fn(&mut Client); 3] {
    [
        |client| client.pane_move_mode = Some(PaneMoveMode::Choice),
        |client| {
            client.pane_move_mode =
                Some(PaneMoveMode::FloatingPaneMove(FloatingPaneMoveSelection {
                    selected_pane_id: PaneId::new(),
                    draft_top_left_cell: None,
                    pending_move_command_id: None,
                }));
        },
        |client| client.dock_undock_mode = Some(DockUndockMode::default()),
    ]
}

#[test]
fn a_restart_during_a_local_mode_holds_nothing() {
    for enter_local_mode in list_local_mode_entries() {
        let mut client = build_test_client();
        enter_local_mode(&mut client);
        let mut restart_recovery = LinkRecovery::RestartRejoin {
            input_hold: Some(InputHold::default()),
        };

        restart_recovery.hold_input(&mut client, HeldInput::Paste(String::from("typed")));

        let LinkRecovery::RestartRejoin { input_hold } = restart_recovery else {
            panic!("holding never changes the recovery");
        };
        assert_eq!(
            input_hold.map(|input_hold| input_hold.held_inputs),
            Some(Vec::new())
        );
    }
}

/// Send `client` a `t` release and the paste `typed`, and assert that neither
/// reaches the session: the first request read back is the `Discovery`
/// sentinel sent after both.
fn assert_key_release_and_paste_stay_in_the_viewer(mut client: Client) {
    let client_id = client.get_client_id();
    let mut control_socket = build_test_control_socket();

    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: build_key_release_for_chord(build_plain_chord('t')),
        },
        Instant::now(),
    );
    process_runtime_input(
        &mut client,
        &mut control_socket.uplink,
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: String::from("typed"),
        },
        Instant::now(),
    );

    // The sentinel goes out behind the release and the paste. A release or a
    // paste that was sent reads back ahead of it.
    control_socket
        .uplink
        .send_request(IpcRequestKind::Discovery);
    let request: IpcRequest = control_socket
        .session_connection
        .recv()
        .expect("read the first request");
    assert_eq!(
        request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Discovery,
        }
    );
}

#[test]
fn the_local_modes_keep_key_releases_and_pastes_from_the_pane() {
    for enter_local_mode in list_local_mode_entries() {
        let mut client = build_test_client();
        enter_local_mode(&mut client);
        assert_key_release_and_paste_stay_in_the_viewer(client);
    }
}

#[test]
fn losing_the_outer_terminals_focus_ends_a_local_mode() {
    for enter_local_mode in list_local_mode_entries() {
        let mut client = build_test_client();
        let client_id = client.get_client_id();
        enter_local_mode(&mut client);
        let mut screen = build_test_screen();
        let (request_sender, _request_receiver) = mpsc::channel();
        let mut uplink = build_test_uplink(request_sender);
        let (incoming_sender, incoming_receiver) = build_incoming_channel();
        for runtime_event in [
            RuntimeEvent::OuterTerminalFocusLost { client_id },
            RuntimeEvent::Quit,
        ] {
            incoming_sender
                .send(build_input_incoming(runtime_event, Instant::now()))
                .expect("the attachment loop accepts the input");
        }
        let mut terminal_mode_bytes: &mut [u8] = &mut [];

        let attachment_ending = run_attachment(
            &build_local_home(),
            SessionId::new(),
            client_id,
            ConnectionToken::generate(),
            None,
            &mut client,
            &mut screen,
            &mut uplink,
            TEXT_ONLY_ATTACH_SETTINGS,
            &mut terminal::CellSizeQuery::from_current_measurement(None, false, false),
            &mut terminal_mode_bytes,
            incoming_sender,
            incoming_receiver,
        );

        assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
        assert_eq!(
            (client.pane_move_mode, client.dock_undock_mode),
            (None, None)
        );
    }
}

#[test]
fn losing_the_outer_terminals_focus_while_redialing_ends_a_local_mode() {
    for enter_local_mode in list_local_mode_entries() {
        let mut viewer = TestViewerWithoutLink::new();
        enter_local_mode(&mut viewer.client);
        let client_id = viewer.client.get_client_id();
        viewer.queue_incoming_event(build_input_incoming(
            RuntimeEvent::OuterTerminalFocusLost { client_id },
            Instant::now(),
        ));
        viewer.queue_incoming_event(viewer.build_key_press_event(build_quit_chord()));
        let (attempt_rejoin, _attempt_count) = build_counted_attempt();

        let stretch_result = viewer.run_without_link(
            LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
            Vec::new(),
            attempt_rejoin,
        );

        assert_eq!(
            stretch_result.err(),
            Some(AttachmentEnding::LeftWhileReconnecting)
        );
        assert_eq!(
            (viewer.client.pane_move_mode, viewer.client.dock_undock_mode),
            (None, None)
        );
    }
}

#[test]
fn quitting_while_a_redial_waits_leaves_before_any_dial() {
    let mut viewer = TestViewerWithoutLink::new();
    viewer.queue_incoming_event(viewer.build_key_press_event(build_quit_chord()));
    let (attempt_rejoin, attempt_count) = build_counted_attempt();
    let link_dropped_at = Instant::now();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(link_dropped_at)),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
    assert!(link_dropped_at.elapsed() < FIRST_REDIAL_WAIT_DURATION);
    assert_eq!(attempt_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        find_drawn_reconnecting_tag(&viewer.screen).as_deref(),
        Some("RECONNECTING (attempt 1, retry in 1s)")
    );
    assert_eq!(viewer.client.link_status, LinkStatus::Linked);
}

#[test]
fn quitting_while_a_dial_runs_leaves_at_once_and_the_tag_reads_dialing() {
    let mut viewer = TestViewerWithoutLink::new();
    let (attempt_rejoin, attempt_start_receiver, attempt_release_sender) = build_blocked_attempt();
    let quit_event = viewer.build_key_press_event(build_quit_chord());
    let quit_sender = viewer.incoming_sender.clone();
    let quit_thread = thread::spawn(move || {
        attempt_start_receiver.recv().expect("the due dial starts");
        quit_sender
            .send(quit_event)
            .expect("the loop's channel takes it");
    });

    let stretch_started_at = Instant::now();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(build_due_redial_schedule()),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
    assert!(
        stretch_started_at.elapsed() < BLOCKED_ATTEMPT_WAIT_DURATION / 2,
        "the stretch left while the dial still ran"
    );
    assert_eq!(
        find_drawn_reconnecting_tag(&viewer.screen).as_deref(),
        Some("RECONNECTING (attempt 1, dialing)")
    );
    quit_thread.join().expect("the quit was sent");
    drop(attempt_release_sender);
}

#[test]
fn a_locked_viewer_leaves_with_the_quit_keybinding_while_reconnecting() {
    let mut viewer = TestViewerWithoutLink::new();
    viewer.client.set_lock_mode(LockMode::Locked);
    viewer.queue_incoming_event(viewer.build_key_press_event(build_quit_chord()));
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
}

#[test]
fn a_quit_left_in_the_batch_that_ended_the_link_leaves_before_any_dial() {
    let mut viewer = TestViewerWithoutLink::new();
    let leftover_quit_event = viewer.build_key_press_event(build_quit_chord());
    let (attempt_rejoin, attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        vec![leftover_quit_event],
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
    assert_eq!(attempt_count.load(Ordering::SeqCst), 0);
}

#[test]
fn quitting_while_a_placement_waits_for_its_answer_leaves_with_the_base_keymap() {
    let mut viewer = TestViewerWithoutLink::new();
    viewer.client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: PaneId::new(),
        source_tab_id: Some(TabId::new()),
        destination_tab_id: TabId::new(),
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: Some(CommandId::new()),
    });
    assert!(viewer.client.is_placement_confirmation_pending());
    viewer.queue_incoming_event(viewer.build_key_press_event(build_quit_chord()));
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
}

#[test]
fn a_quit_bound_as_a_key_sequence_leaves_while_reconnecting() {
    let mut viewer = TestViewerWithoutLink::new();
    let pane_group_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p'));
    bind_quit_in_normal_mode(
        &mut viewer.client,
        KeySequence::from_first_and_rest(pane_group_chord, vec![build_plain_chord('z')]),
    );
    viewer.queue_incoming_event(viewer.build_key_press_event(pane_group_chord));
    viewer.queue_incoming_event(viewer.build_key_press_event(build_plain_chord('z')));
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
}

#[test]
fn a_quit_on_an_ambiguous_prefix_leaves_when_its_deadline_passes_while_reconnecting() {
    let mut viewer = TestViewerWithoutLink::new();
    let pane_group_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p'));
    bind_quit_in_normal_mode(&mut viewer.client, KeySequence::from(pane_group_chord));
    let chord_pressed_at =
        compute_instant_before_now(viewer.client.keymap_catalog.get_chord_timeout());
    assert_eq!(
        viewer
            .client
            .resolve_key(pane_group_chord, chord_pressed_at),
        KeyOutcome::Pending,
        "the quit chord opens longer pane bindings too"
    );
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
}

#[test]
fn keys_resizes_and_stale_frames_while_redialing_send_nothing_and_a_hangup_ends_the_stretch() {
    let mut viewer = TestViewerWithoutLink::new();
    let sequence_opener_chord = get_sequence_opener(&viewer.client);
    viewer.queue_incoming_event(viewer.build_key_press_event(build_plain_chord('a')));
    viewer.queue_incoming_event(viewer.build_key_press_event(sequence_opener_chord));
    viewer.queue_incoming_event(build_input_incoming(
        RuntimeEvent::Resize {
            client_id: viewer.client.get_client_id(),
            viewport_size: Size {
                column_count: 100,
                row_count: 40,
            },
            pane_area: None,
            cell_size: None,
        },
        Instant::now(),
    ));
    viewer.queue_incoming_event(Incoming::Painted {
        connection_index: INITIAL_CONNECTION_INDEX,
        received_painted_frame: Box::new(build_received_painted_frame(build_test_painted_frame())),
    });
    viewer.queue_incoming_event(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
    let (attempt_rejoin, attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(stretch_result.err(), Some(AttachmentEnding::TerminalGone));
    assert_eq!(viewer.client.get_viewport_size(), TEST_VIEWPORT_SIZE);
    assert_eq!(
        viewer.client.get_pending_key_sequence(),
        Some(&KeySequence::from(sequence_opener_chord)),
        "the sequence opener keeps its breadcrumb"
    );
    assert_eq!(
        find_drawn_reconnecting_tag(&viewer.screen).as_deref(),
        Some("RECONNECTING (attempt 1, retry in 1s)")
    );
    assert_eq!(attempt_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        viewer.incoming_receiver.try_recv().err(),
        Some(mpsc::TryRecvError::Empty),
        "every queued event was taken off the channel"
    );
}

#[test]
fn a_redial_that_joins_hands_back_the_session_and_holds_nothing_typed() {
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    viewer.queue_incoming_event(viewer.build_key_press_event(build_plain_chord('a')));
    viewer.queue_incoming_event(build_input_incoming(
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: String::from("pasted"),
        },
        Instant::now(),
    ));
    let (joined_answer, mut session_connection) = build_joined_answer(client_id);
    viewer.queue_incoming_event(joined_answer);
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        Vec::new(),
        attempt_rejoin,
    );

    let Ok((joined_session, held_inputs)) = stretch_result else {
        panic!("the queued join ends the stretch");
    };
    assert_eq!(joined_session.client_id, client_id);
    assert_eq!(held_inputs, Vec::new());
    assert_eq!(viewer.client.link_status, LinkStatus::Linked);
    assert_eq!(
        find_drawn_reconnecting_tag(&viewer.screen),
        None,
        "the join draws the screen once more without the tag"
    );
    // The loop took the join: closing it sends no `Leaving`.
    drop(joined_session);
    let Err(IpcError::Disconnected) = session_connection.recv::<IpcRequest>() else {
        panic!("a join the loop took closes with no request on it");
    };
}

#[test]
fn an_unreachable_redial_schedules_the_next_dial_with_a_wider_pause() {
    let mut viewer = TestViewerWithoutLink::new();
    let running_dial_schedule = RedialSchedule {
        started_at: Instant::now(),
        attempt: 1,
        retry_wait: FIRST_REDIAL_WAIT_DURATION,
        next_dial_at: None,
    };
    let leftover_events = vec![
        Incoming::RejoinAnswer(Box::new(RejoinAnswer::Unreachable(
            CliError::IpcUnavailable {
                detail: String::from("connection refused"),
            },
        ))),
        viewer.build_key_press_event(build_quit_chord()),
    ];
    let (attempt_rejoin, attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(running_dial_schedule),
        leftover_events,
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
    assert_eq!(
        find_drawn_reconnecting_tag(&viewer.screen).as_deref(),
        Some("RECONNECTING (attempt 2, retry in 2s)")
    );
    assert_eq!(attempt_count.load(Ordering::SeqCst), 0);
}

#[test]
fn an_unreachable_redial_whose_next_pause_passes_the_window_ends_with_its_cause() {
    let mut viewer = TestViewerWithoutLink::new();
    let last_dial_schedule = RedialSchedule {
        started_at: compute_instant_before_now(Duration::from_secs(119)),
        attempt: 17,
        retry_wait: MAX_REDIAL_WAIT_DURATION,
        next_dial_at: None,
    };
    let leftover_events = vec![Incoming::RejoinAnswer(Box::new(RejoinAnswer::Unreachable(
        CliError::IpcUnavailable {
            detail: String::from("connection refused"),
        },
    )))];
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(last_dial_schedule),
        leftover_events,
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LinkLost(Box::new(
            CliError::IpcUnavailable {
                detail: String::from("connection refused"),
            }
        )))
    );
}

#[test]
fn a_refused_redial_ends_the_stretch_at_once_with_that_refusal() {
    let mut viewer = TestViewerWithoutLink::new();
    let attempt_rejoin: Arc<dyn Fn() -> RejoinAnswer + Send + Sync> = Arc::new(|| {
        build_redial_answer(Err(DialError::Refused(CliError::Runtime {
            detail: String::from("the server host:7654 did not admit the connection"),
        })))
    });

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(build_due_redial_schedule()),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LinkLost(Box::new(CliError::Runtime {
            detail: String::from("the server host:7654 did not admit the connection"),
        })))
    );
}

#[test]
fn keys_and_pastes_during_a_restart_are_held_in_order_for_the_restarted_session() {
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    viewer.queue_incoming_event(viewer.build_key_press_event(build_plain_chord('a')));
    viewer.queue_incoming_event(build_input_incoming(
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: String::from("pasted"),
        },
        Instant::now(),
    ));
    viewer.queue_incoming_event(viewer.build_key_press_event(build_plain_chord('b')));
    let (joined_answer, _session_connection) = build_joined_answer(client_id);
    viewer.queue_incoming_event(joined_answer);
    let (attempt_rejoin, _attempt_start_receiver, _attempt_release_sender) =
        build_blocked_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::RestartRejoin {
            input_hold: Some(InputHold::default()),
        },
        Vec::new(),
        attempt_rejoin,
    );

    let Ok((_joined_session, held_inputs)) = stretch_result else {
        panic!("the queued join ends the stretch");
    };
    assert_eq!(
        held_inputs,
        vec![
            HeldInput::Keyboard(crate::tests::build_key_input_for_chord(build_plain_chord(
                'a'
            ))),
            HeldInput::Paste(String::from("pasted")),
            HeldInput::Keyboard(crate::tests::build_key_input_for_chord(build_plain_chord(
                'b'
            ))),
        ]
    );
}

#[test]
fn held_inputs_go_up_in_the_order_they_were_typed() {
    let mut client = build_test_client();
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);

    send_held_inputs(
        &mut client,
        &mut uplink,
        vec![
            HeldInput::Keyboard(crate::tests::build_key_input_for_chord(build_plain_chord(
                'a',
            ))),
            HeldInput::DispatchPlan(DispatchPlan::Command(Box::new(Command::NewTab(
                NewTabArgs::default(),
            )))),
            HeldInput::Paste(String::from("pasted")),
        ],
    );

    let sent_requests: Vec<IpcRequest> = request_receiver.try_iter().collect();
    assert_eq!(sent_requests.len(), 3);
    assert_eq!(
        sent_requests[0].request_kind,
        IpcRequestKind::Keyboard {
            key_input: crate::tests::build_key_input_for_chord(build_plain_chord('a')),
        }
    );
    let IpcRequestKind::SubmitCommand(command_envelope) = &sent_requests[1].request_kind else {
        panic!("the held binding goes up as its command");
    };
    assert_eq!(
        command_envelope.command,
        Command::NewTab(NewTabArgs::default())
    );
    assert_eq!(
        sent_requests[2].request_kind,
        IpcRequestKind::Paste {
            pasted_text: String::from("pasted"),
        }
    );
}

#[test]
fn an_overflowing_restart_hold_still_leaves_on_the_quit_keybinding() {
    let mut viewer = TestViewerWithoutLink::new();
    let mut leftover_events: Vec<Incoming> = (0..=MAX_HELD_INPUT_COUNT)
        .map(|_| viewer.build_key_press_event(build_plain_chord('a')))
        .collect();
    leftover_events.push(viewer.build_key_press_event(build_quit_chord()));
    let (attempt_rejoin, _attempt_start_receiver, _attempt_release_sender) =
        build_blocked_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::RestartRejoin {
            input_hold: Some(InputHold::default()),
        },
        leftover_events,
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
}

#[test]
fn a_restart_whose_hold_overflowed_joins_with_nothing_held() {
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    let leftover_events: Vec<Incoming> = (0..=MAX_HELD_INPUT_COUNT)
        .map(|_| viewer.build_key_press_event(build_plain_chord('a')))
        .collect();
    let (joined_answer, _session_connection) = build_joined_answer(client_id);
    viewer.queue_incoming_event(joined_answer);
    let (attempt_rejoin, _attempt_start_receiver, _attempt_release_sender) =
        build_blocked_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::RestartRejoin {
            input_hold: Some(InputHold::default()),
        },
        leftover_events,
        attempt_rejoin,
    );

    let Ok((_joined_session, held_inputs)) = stretch_result else {
        panic!("the queued join ends the stretch");
    };
    assert_eq!(held_inputs, Vec::new());
}

#[test]
fn keys_typed_after_the_lock_key_resolve_in_locked_mode_before_the_session_answers() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);

    for key_chord in [build_lock_chord(), build_tab_chord()] {
        process_runtime_input(
            &mut client,
            &mut uplink,
            RuntimeEvent::KeyInput {
                client_id,
                key_input: crate::tests::build_key_input_for_chord(key_chord),
            },
            Instant::now(),
        );
    }

    let sent_request_kinds: Vec<IpcRequestKind> = request_receiver
        .try_iter()
        .map(|sent_request| sent_request.request_kind)
        .collect();
    let [IpcRequestKind::SubmitCommand(lock_command_envelope), tab_request_kind] =
        sent_request_kinds.as_slice()
    else {
        panic!("the lock key and Tab send two requests, not {sent_request_kinds:?}");
    };
    assert_eq!(
        lock_command_envelope.command,
        build_expected_lock_command(true)
    );
    assert_eq!(
        lock_command_envelope.command_source,
        CommandSource::from_key_binding(client_id)
    );
    assert_eq!(
        *tab_request_kind,
        IpcRequestKind::Keyboard {
            key_input: crate::tests::build_key_input_for_chord(build_tab_chord()),
        }
    );
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(
        client.get_pending_lock_command_id(),
        Some(lock_command_envelope.command_id)
    );
}

#[test]
fn a_quit_key_bound_in_normal_mode_alone_reaches_the_pane_right_after_the_lock_key() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let quit_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('x'));
    bind_quit_in_normal_mode(&mut client, KeySequence::from(quit_chord));
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);

    for key_chord in [build_lock_chord(), quit_chord] {
        process_runtime_input(
            &mut client,
            &mut uplink,
            RuntimeEvent::KeyInput {
                client_id,
                key_input: crate::tests::build_key_input_for_chord(key_chord),
            },
            Instant::now(),
        );
    }

    let sent_request_kinds: Vec<IpcRequestKind> = request_receiver
        .try_iter()
        .map(|sent_request| sent_request.request_kind)
        .collect();
    let [IpcRequestKind::SubmitCommand(lock_command_envelope), quit_key_request_kind] =
        sent_request_kinds.as_slice()
    else {
        panic!("the lock key and <C-x> send two requests, not {sent_request_kinds:?}");
    };
    assert_eq!(
        lock_command_envelope.command,
        build_expected_lock_command(true)
    );
    assert_eq!(
        *quit_key_request_kind,
        IpcRequestKind::Keyboard {
            key_input: crate::tests::build_key_input_for_chord(quit_chord),
        }
    );
}

#[test]
fn a_toggle_lock_binding_goes_up_as_the_lock_mode_it_sets() {
    let mut client = build_test_client();
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let toggle_lock_binding = build_core_bound_action("toggle-lock");

    assert_eq!(
        uplink.submit_bound_action(&mut client, toggle_lock_binding.clone(), None),
        ControlFlow::Continue(())
    );
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(
        uplink.submit_bound_action(&mut client, toggle_lock_binding, None),
        ControlFlow::Continue(())
    );

    let sent_commands: Vec<(CommandId, Command)> = request_receiver
        .try_iter()
        .map(|sent_request| match sent_request.request_kind {
            IpcRequestKind::SubmitCommand(command_envelope) => {
                (command_envelope.command_id, command_envelope.command)
            }
            other_request_kind => panic!("a toggle sends a command, not {other_request_kind:?}"),
        })
        .collect();
    assert_eq!(
        sent_commands
            .iter()
            .map(|(_, command)| command.clone())
            .collect::<Vec<Command>>(),
        vec![
            build_expected_lock_command(true),
            build_expected_lock_command(false)
        ]
    );
    assert_ne!(sent_commands[0].0, sent_commands[1].0);
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert_eq!(
        client.get_pending_lock_command_id(),
        Some(sent_commands[1].0)
    );
}

#[test]
fn lock_keys_during_a_restart_switch_the_mode_the_next_key_resolves_in() {
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    for key_chord in [
        build_lock_chord(),
        build_tab_chord(),
        build_lock_chord(),
        build_tab_chord(),
    ] {
        viewer.queue_incoming_event(viewer.build_key_press_event(key_chord));
    }
    let (joined_answer, _session_connection) = build_joined_answer(client_id);
    viewer.queue_incoming_event(joined_answer);
    let (attempt_rejoin, _attempt_start_receiver, _attempt_release_sender) =
        build_blocked_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::RestartRejoin {
            input_hold: Some(InputHold::default()),
        },
        Vec::new(),
        attempt_rejoin,
    );

    let Ok((_joined_session, held_inputs)) = stretch_result else {
        panic!("the queued join ends the stretch");
    };
    let [HeldInput::LockCommand {
        lock_command_id: first_lock_command_id,
        lock_mode: LockMode::Locked,
    }, HeldInput::Keyboard(locked_tab_key_input), HeldInput::LockCommand {
        lock_command_id: second_lock_command_id,
        lock_mode: LockMode::Normal,
    }, HeldInput::DispatchPlan(next_tab_plan)] = held_inputs.as_slice()
    else {
        panic!("lock, Tab, unlock and Tab are held as they resolved, not {held_inputs:?}");
    };
    assert_ne!(first_lock_command_id, second_lock_command_id);
    assert_eq!(
        *locked_tab_key_input,
        crate::tests::build_key_input_for_chord(build_tab_chord())
    );
    assert_eq!(
        *next_tab_plan,
        DispatchPlan::Command(Box::new(Command::FocusTab(FocusTabArgs {
            focus_target: TabTarget::Next,
            client_id: None,
        })))
    );
    assert_eq!(viewer.client.get_lock_mode(), LockMode::Normal);
    assert_eq!(
        viewer.client.get_pending_lock_command_id(),
        Some(*second_lock_command_id)
    );
}

#[test]
fn a_lock_key_during_a_redial_is_dropped_and_switches_nothing() {
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    viewer.queue_incoming_event(viewer.build_key_press_event(build_lock_chord()));
    let (joined_answer, _session_connection) = build_joined_answer(client_id);
    viewer.queue_incoming_event(joined_answer);
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        Vec::new(),
        attempt_rejoin,
    );

    let Ok((_joined_session, held_inputs)) = stretch_result else {
        panic!("the queued join ends the stretch");
    };
    assert_eq!(held_inputs, Vec::new());
    assert_eq!(viewer.client.get_lock_mode(), LockMode::Normal);
    assert_eq!(viewer.client.get_pending_lock_command_id(), None);
}

#[test]
fn a_binding_the_action_table_refuses_is_held_as_nothing() {
    let mut client = build_test_client();
    let (request_sender, _request_receiver) = mpsc::channel();
    let uplink = build_test_uplink(request_sender);
    let mut link_recovery = LinkRecovery::RestartRejoin {
        input_hold: Some(InputHold::default()),
    };
    let unregistered_binding = build_core_bound_action("no-such-action");

    assert_eq!(
        hold_fired_binding(
            &mut link_recovery,
            &mut client,
            &uplink,
            unregistered_binding
        ),
        ControlFlow::Continue(())
    );

    let LinkRecovery::RestartRejoin { input_hold } = link_recovery else {
        panic!("holding never changes the recovery");
    };
    assert_eq!(
        input_hold.map(|input_hold| input_hold.held_inputs),
        Some(Vec::new())
    );
}

#[test]
fn an_overflowing_hold_puts_back_the_lock_mode_from_before_its_first_lock_command() {
    let mut client = build_test_client();
    let lock_command_id_sent_before_the_restart = CommandId::new();
    client.apply_lock_command(lock_command_id_sent_before_the_restart, LockMode::Locked);
    let mut link_recovery = LinkRecovery::RestartRejoin {
        input_hold: Some(InputHold::default()),
    };
    let held_unlock_command_id = CommandId::new();
    link_recovery.hold_input(
        &mut client,
        HeldInput::LockCommand {
            lock_command_id: held_unlock_command_id,
            lock_mode: LockMode::Normal,
        },
    );
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert_eq!(
        client.get_pending_lock_command_id(),
        Some(held_unlock_command_id)
    );
    let held_lock_command_id = CommandId::new();
    link_recovery.hold_input(
        &mut client,
        HeldInput::LockCommand {
            lock_command_id: held_lock_command_id,
            lock_mode: LockMode::Locked,
        },
    );
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(
        client.get_pending_lock_command_id(),
        Some(held_lock_command_id)
    );

    for _ in 0..MAX_HELD_INPUT_COUNT - 1 {
        link_recovery.hold_input(
            &mut client,
            HeldInput::Keyboard(crate::tests::build_key_input_for_chord(build_plain_chord(
                'a',
            ))),
        );
    }

    let LinkRecovery::RestartRejoin { input_hold } = link_recovery else {
        panic!("holding never changes the recovery");
    };
    assert_eq!(input_hold.map(|input_hold| input_hold.held_inputs), None);
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(
        client.get_pending_lock_command_id(),
        Some(lock_command_id_sent_before_the_restart)
    );
}

#[test]
fn held_lock_commands_go_up_under_their_own_ids_and_leave_the_mode_and_open_sequence_alone() {
    let mut client = build_test_client();
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    client.apply_lock_command(CommandId::new(), LockMode::Normal);
    let open_sequence_chord = KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('p'));
    assert_eq!(
        client.resolve_key(open_sequence_chord, Instant::now()),
        KeyOutcome::Pending
    );
    let first_lock_command_id = CommandId::new();
    let second_lock_command_id = CommandId::new();

    send_held_inputs(
        &mut client,
        &mut uplink,
        vec![
            HeldInput::LockCommand {
                lock_command_id: first_lock_command_id,
                lock_mode: LockMode::Locked,
            },
            HeldInput::Keyboard(crate::tests::build_key_input_for_chord(build_tab_chord())),
            HeldInput::LockCommand {
                lock_command_id: second_lock_command_id,
                lock_mode: LockMode::Normal,
            },
        ],
    );

    let sent_requests: Vec<IpcRequest> = request_receiver.try_iter().collect();
    let [first_lock_request, tab_request, second_lock_request] = sent_requests.as_slice() else {
        panic!("three held inputs send three requests, not {sent_requests:?}");
    };
    let IpcRequestKind::SubmitCommand(first_lock_envelope) = &first_lock_request.request_kind
    else {
        panic!("a held lock command goes up as a command");
    };
    assert_eq!(first_lock_envelope.command_id, first_lock_command_id);
    assert_eq!(
        first_lock_envelope.command,
        build_expected_lock_command(true)
    );
    assert_eq!(
        tab_request.request_kind,
        IpcRequestKind::Keyboard {
            key_input: crate::tests::build_key_input_for_chord(build_tab_chord()),
        }
    );
    let IpcRequestKind::SubmitCommand(second_lock_envelope) = &second_lock_request.request_kind
    else {
        panic!("a held lock command goes up as a command");
    };
    assert_eq!(second_lock_envelope.command_id, second_lock_command_id);
    assert_eq!(
        second_lock_envelope.command,
        build_expected_lock_command(false)
    );
    assert_eq!(client.get_lock_mode(), LockMode::Normal);
    assert_eq!(
        client.get_pending_lock_command_id(),
        Some(second_lock_command_id)
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from(open_sequence_chord))
    );
}

#[test]
fn a_new_connection_with_no_held_lock_command_waits_for_no_lock_command() {
    let mut client = build_test_client();
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    client.apply_lock_command(CommandId::new(), LockMode::Locked);

    send_held_inputs(&mut client, &mut uplink, Vec::new());

    assert_eq!(request_receiver.try_iter().count(), 0);
    assert_eq!(client.get_lock_mode(), LockMode::Locked);
    assert_eq!(client.get_pending_lock_command_id(), None);
}

#[test]
fn a_join_read_before_a_quit_hands_the_quit_to_the_new_link() {
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    let (joined_answer, _session_connection) = build_joined_answer(client_id);
    viewer.queue_incoming_event(joined_answer);
    viewer.queue_incoming_event(viewer.build_key_press_event(build_quit_chord()));
    let (attempt_rejoin, _attempt_start_receiver, _attempt_release_sender) =
        build_blocked_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::RestartRejoin {
            input_hold: Some(InputHold::default()),
        },
        Vec::new(),
        attempt_rejoin,
    );

    let Ok((joined_session, held_inputs)) = stretch_result else {
        panic!("the join was read first");
    };
    assert_eq!(joined_session.client_id, client_id);
    assert_eq!(held_inputs, Vec::new());
    let Ok(Incoming::Input(terminal_input)) = viewer.incoming_receiver.try_recv() else {
        panic!("the quit stays on the channel for the linked loop");
    };
    let RuntimeEvent::KeyInput { key_input, .. } = terminal_input.runtime_event else {
        panic!("the quit stays a key");
    };
    assert_eq!(
        key_input,
        crate::tests::build_key_input_for_chord(build_quit_chord())
    );
}

#[test]
fn a_quit_read_before_a_join_leaves_and_closes_the_joined_connection() {
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    viewer.queue_incoming_event(viewer.build_key_press_event(build_quit_chord()));
    let (joined_answer, mut session_connection) = build_joined_answer(client_id);
    viewer.queue_incoming_event(joined_answer);
    let (attempt_rejoin, _attempt_start_receiver, _attempt_release_sender) =
        build_blocked_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::RestartRejoin {
            input_hold: Some(InputHold::default()),
        },
        Vec::new(),
        attempt_rejoin,
    );
    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
    drop(viewer);

    assert_joined_connection_leaves_then_closes(&mut session_connection);
}

#[test]
fn a_join_whose_attempt_finds_the_channel_gone_leaves_and_closes_the_joined_connection() {
    let (joined_session, mut session_connection) = build_joined_session(ClientId::new());
    let unanswered_joined_session = Mutex::new(Some(joined_session));
    let attempt_rejoin: Arc<dyn Fn() -> RejoinAnswer + Send + Sync> = Arc::new(move || {
        let joined_session = unanswered_joined_session
            .lock()
            .expect("the join outlives every panic")
            .take()
            .expect("the attempt runs once");
        RejoinAnswer::Joined(JoinedSessionGuard::from_joined_session(joined_session))
    });
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    drop(incoming_receiver);

    spawn_rejoin_attempt(attempt_rejoin, incoming_sender);

    assert_joined_connection_leaves_then_closes(&mut session_connection);
}

#[test]
fn a_rejoin_attempt_that_panics_ends_the_stretch_as_a_lost_link() {
    let mut viewer = TestViewerWithoutLink::new();
    let attempt_rejoin: Arc<dyn Fn() -> RejoinAnswer + Send + Sync> =
        Arc::new(|| panic!("the attempt broke"));

    let stretch_result = viewer.run_without_link(
        LinkRecovery::RestartRejoin {
            input_hold: Some(InputHold::default()),
        },
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LinkLost(Box::new(CliError::Runtime {
            detail: String::from(
                "the attempt to reach the session again stopped on an internal error"
            ),
        })))
    );
}

#[test]
fn a_cell_size_reply_while_reconnecting_is_taken_by_the_query() {
    let mut viewer = TestViewerWithoutLink::new();
    viewer.cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, true, true);
    let measured_cell_size = koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
        .expect("positive cell dimensions");
    let leftover_events = vec![
        build_input_incoming(
            RuntimeEvent::CellSize {
                client_id: viewer.client.get_client_id(),
                cell_size: measured_cell_size,
            },
            Instant::now(),
        ),
        viewer.build_key_press_event(build_quit_chord()),
    ];
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        leftover_events,
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting)
    );
    assert_eq!(
        viewer
            .cell_size_query
            .accept_cell_size_reply(measured_cell_size),
        (None, false),
        "the stretch took the reply instead of leaving it pending"
    );
}

#[test]
fn quitting_waits_for_a_stalled_writer_no_longer_than_the_quit_write_wait() {
    let (request_sender, _request_receiver) = mpsc::channel();
    let (_writer_end_sender, writer_end_receiver) = mpsc::channel::<()>();
    let mut uplink = Uplink {
        uplink_writer: Some(UplinkWriter {
            request_sender,
            writer_end_receiver,
        }),
        registry: ActionRegistry::new(),
        next_request_id: FIRST_POST_ATTACH_REQUEST_ID,
        sent_placement_preview_target: None,
    };
    let quit_started_at = Instant::now();

    assert_eq!(leave_after_quit(&mut uplink), AttachmentEnding::Detached);

    let quit_duration = quit_started_at.elapsed();
    assert!(quit_duration >= MAX_QUIT_WRITE_WAIT_DURATION);
    assert!(quit_duration < MAX_QUIT_WRITE_WAIT_DURATION + Duration::from_secs(1));
}

#[test]
fn quitting_with_a_link_writes_the_quit_and_leaves_without_the_sessions_answer() {
    let mut test_control_socket = build_test_control_socket();
    let mut client = build_test_client();
    let client_id = client.get_client_id();

    assert_eq!(
        process_runtime_input_with_cell_size(
            &mut client,
            &mut test_control_socket.uplink,
            &mut terminal::CellSizeQuery::from_current_measurement(None, false, false),
            None,
            RuntimeEvent::KeyInput {
                client_id,
                key_input: crate::tests::build_key_input_for_chord(build_quit_chord()),
            },
            Instant::now(),
        ),
        ControlFlow::Break(())
    );
    assert_eq!(
        leave_after_quit(&mut test_control_socket.uplink),
        AttachmentEnding::Detached
    );

    let quit_request: IpcRequest = test_control_socket
        .session_connection
        .recv()
        .expect("the quit reached the socket before the client left");
    let IpcRequestKind::SubmitCommand(command_envelope) = quit_request.request_kind else {
        panic!("the quit goes up as a command");
    };
    assert_eq!(command_envelope.command, Command::Quit);
}

#[test]
fn the_quit_keybinding_ends_a_linked_attachment_at_once_as_detached() {
    // The attachment runs on its own thread, so a loop that waits for the
    // session's answer fails the wait below instead of holding the test.
    let (attachment_result_sender, attachment_result_receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut client = build_test_client();
        let client_id = client.get_client_id();
        let mut screen = build_test_screen();
        let (request_sender, request_receiver) = mpsc::channel();
        let mut uplink = build_test_uplink(request_sender);
        let (incoming_sender, incoming_receiver) = build_incoming_channel();
        incoming_sender
            .send(build_input_incoming(
                RuntimeEvent::KeyInput {
                    client_id,
                    key_input: crate::tests::build_key_input_for_chord(build_quit_chord()),
                },
                Instant::now(),
            ))
            .expect("the loop's channel takes it");
        let mut cell_size_query =
            terminal::CellSizeQuery::from_current_measurement(None, false, false);
        let attachment_ending = run_attachment(
            &build_local_home(),
            SessionId::new(),
            client_id,
            ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
            None,
            &mut client,
            &mut screen,
            &mut uplink,
            TEXT_ONLY_ATTACH_SETTINGS,
            &mut cell_size_query,
            &mut Vec::new(),
            incoming_sender,
            incoming_receiver,
        );
        let sent_request_kinds: Vec<IpcRequestKind> = request_receiver
            .try_iter()
            .map(|sent_request| sent_request.request_kind)
            .collect();
        let _ = attachment_result_sender.send((attachment_ending, sent_request_kinds));
    });

    let (attachment_ending, sent_request_kinds) = attachment_result_receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("the attachment ended without an answer from the session");

    assert_eq!(attachment_ending, AttachmentEnding::Detached);
    assert_eq!(sent_request_kinds.len(), 1);
    let IpcRequestKind::SubmitCommand(command_envelope) = &sent_request_kinds[0] else {
        panic!("the quit goes up as a command");
    };
    assert_eq!(command_envelope.command, Command::Quit);
}

#[test]
fn a_remote_viewer_with_reconnect_off_ends_at_once_when_its_link_breaks() {
    let mut client = build_test_client();
    client.client_config.should_reconnect_remote_session = false;
    let client_id = client.get_client_id();
    let mut screen = build_test_screen();
    let (request_sender, _request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    incoming_sender
        .send(Incoming::Frame {
            connection_index: INITIAL_CONNECTION_INDEX,
            session_event_result: Err(IpcError::Disconnected),
        })
        .expect("the loop's channel takes it");
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);

    let attachment_ending = run_attachment(
        &build_remote_home(),
        SessionId::new(),
        client_id,
        ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
        None,
        &mut client,
        &mut screen,
        &mut uplink,
        TEXT_ONLY_ATTACH_SETTINGS,
        &mut cell_size_query,
        &mut Vec::new(),
        incoming_sender,
        incoming_receiver,
    );

    assert_eq!(attachment_ending, AttachmentEnding::ConnectionDied);
    assert_eq!(client.link_status, LinkStatus::Linked);
}

/// Run one local attachment on `screen` over `incoming_events`, then a
/// `Detached` event.
fn run_attachment_over_image_events(
    screen: &mut Screen<TestBackend>,
    incoming_events: Vec<Incoming>,
) {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let (request_sender, _request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    for incoming_event in incoming_events
        .into_iter()
        .chain([build_session_event_incoming(SessionEvent::Detached)])
    {
        incoming_sender
            .send(incoming_event)
            .expect("the loop's channel takes it");
    }
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);

    let attachment_ending = run_attachment(
        &build_local_home(),
        SessionId::new(),
        client_id,
        ConnectionToken::generate(),
        None,
        &mut client,
        screen,
        &mut uplink,
        TEXT_ONLY_ATTACH_SETTINGS,
        &mut cell_size_query,
        &mut Vec::new(),
        incoming_sender,
        incoming_receiver,
    );

    assert_eq!(attachment_ending, AttachmentEnding::Detached);
}

/// The session event `session_event` on the first connection.
fn build_session_event_incoming(session_event: SessionEvent) -> Incoming {
    Incoming::Frame {
        connection_index: INITIAL_CONNECTION_INDEX,
        session_event_result: Ok(session_event),
    }
}

/// The transfer of the 2×1 image `image_content_id`, whose RGBA bytes are 8.
fn build_two_pixel_image_transfer(image_content_id: u64) -> FrameImageTransfer {
    FrameImageTransfer {
        image_content_id,
        image_record: FrameImageRecordHeader {
            protocol: FrameGraphicsProtocol::Kitty,
            pixel_width: 2,
            pixel_height: 1,
            image_action: FrameImageAction::TransmitAndDisplay,
            display: FrameImageDisplay::default(),
            anchor_cell: (0, 0),
        },
        image_byte_count: 8,
    }
}

#[test]
fn the_encode_cache_leaves_room_for_the_image_bytes_each_session_event_leaves_held() {
    let mut started_transfer_screen = build_test_screen();
    run_attachment_over_image_events(
        &mut started_transfer_screen,
        vec![build_session_event_incoming(
            SessionEvent::ImageContentStart {
                image_transfer: build_two_pixel_image_transfer(1),
            },
        )],
    );
    let mut reset_cache_screen = build_test_screen();
    run_attachment_over_image_events(
        &mut reset_cache_screen,
        vec![
            build_session_event_incoming(SessionEvent::ImageContentStart {
                image_transfer: build_two_pixel_image_transfer(1),
            }),
            build_session_event_incoming(SessionEvent::ImageCacheReset),
        ],
    );

    assert_eq!(
        started_transfer_screen
            .image_output_state
            .get_held_image_byte_count(),
        8
    );
    assert_eq!(
        reset_cache_screen
            .image_output_state
            .get_held_image_byte_count(),
        0
    );
}

#[test]
fn a_released_image_leaves_the_encode_cache_and_gives_its_bytes_back() {
    let mut screen = build_test_screen();
    screen
        .image_output_state
        .keep_kitty_payload(1, Arc::from(vec![1, 2, 3]));
    screen
        .image_output_state
        .keep_kitty_payload(2, Arc::from(vec![4, 5, 6]));

    run_attachment_over_image_events(
        &mut screen,
        vec![
            build_session_event_incoming(SessionEvent::ImageContentStart {
                image_transfer: build_two_pixel_image_transfer(1),
            }),
            Incoming::ImageChunk {
                connection_index: INITIAL_CONNECTION_INDEX,
                image_chunk_frame: build_image_chunk_frame(1, 0, true, &[7; 8]),
            },
            build_session_event_incoming(SessionEvent::ImageContentReleased {
                image_content_ids: vec![1],
            }),
        ],
    );

    assert_eq!(
        screen
            .image_output_state
            .list_kept_kitty_payload_content_ids(),
        [2]
    );
    assert_eq!(screen.image_output_state.get_held_image_byte_count(), 0);
}

#[test]
fn leaving_while_reconnecting_exits_zero_and_names_no_next_session() {
    let report_result = report_attachment_ending(
        &build_remote_home(),
        AttachmentEnding::LeftWhileReconnecting,
        SessionId::new(),
        None,
    );

    let Ok(next_session_id) = report_result else {
        panic!("leaving while reconnecting is no error");
    };
    assert_eq!(next_session_id, None);
}

#[test]
fn incoming_batch_leaves_events_past_the_per_pass_limit_queued() {
    let (incoming_sender, incoming_receiver) = mpsc::channel();
    for _ in 0..MAX_INCOMING_EVENT_COUNT_PER_PASS {
        incoming_sender
            .send(build_input_incoming(RuntimeEvent::Quit, Instant::now()))
            .expect("the test receiver is open");
    }
    let client_id = ClientId::new();
    incoming_sender
        .send(build_input_incoming(
            RuntimeEvent::Resize {
                client_id,
                viewport_size: Size {
                    column_count: 90,
                    row_count: 30,
                },
                pane_area: None,
                cell_size: None,
            },
            Instant::now(),
        ))
        .expect("the test receiver is open");

    let (incoming_events, deferred_incoming_event) = build_incoming_batch(None, &incoming_receiver);

    assert_eq!(incoming_events.len(), MAX_INCOMING_EVENT_COUNT_PER_PASS);
    let None = deferred_incoming_event else {
        panic!("a full event-count batch must leave the next event in the channel");
    };
    for incoming_event in incoming_events {
        let Incoming::Input(terminal_input) = incoming_event else {
            panic!("the batch contained a session event");
        };
        match terminal_input.runtime_event {
            RuntimeEvent::Quit => {}
            unexpected_runtime_event => panic!("the batch contained {unexpected_runtime_event:?}"),
        }
    }
    let remaining_incoming_event = incoming_receiver
        .try_recv()
        .expect("the event past the count limit remains queued");
    let Incoming::Input(terminal_input) = remaining_incoming_event else {
        panic!("the remaining event came from the session");
    };
    match terminal_input.runtime_event {
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
        incoming_receiver.try_recv().err(),
        Some(mpsc::TryRecvError::Empty)
    );
}

#[test]
fn incoming_queue_stops_producers_at_its_fixed_capacity() {
    assert_eq!(INCOMING_QUEUE_CAPACITY, 16);
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    for _ in 0..INCOMING_QUEUE_CAPACITY {
        incoming_sender
            .try_send(build_input_incoming(RuntimeEvent::Quit, Instant::now()))
            .expect("an event inside the queue limit fits");
    }
    let client_id = ClientId::new();
    let rejected_send_result = incoming_sender.try_send(build_input_incoming(
        RuntimeEvent::Resize {
            client_id,
            viewport_size: Size {
                column_count: 90,
                row_count: 30,
            },
            pane_area: None,
            cell_size: None,
        },
        Instant::now(),
    ));

    let Err(mpsc::TrySendError::Full(Incoming::Input(terminal_input))) = rejected_send_result
    else {
        panic!("the event past the queue limit was not returned as full");
    };
    let RuntimeEvent::Resize {
        client_id: actual_client_id,
        viewport_size,
        pane_area,
        cell_size,
    } = terminal_input.runtime_event
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
        let Incoming::Input(terminal_input) = incoming_receiver
            .try_recv()
            .expect("each accepted event remains queued")
        else {
            panic!("the queue contained a session event");
        };
        let RuntimeEvent::Quit = terminal_input.runtime_event else {
            panic!("the queue contained another input event");
        };
    }
    assert_eq!(
        incoming_receiver.try_recv().err(),
        Some(mpsc::TryRecvError::Empty)
    );
}

#[test]
fn terminal_input_queue_stops_the_reader_at_its_fixed_capacity() {
    assert_eq!(INCOMING_QUEUE_CAPACITY, 16);
    let (input_sender, input_receiver) = build_input_channel();
    let read_at = Instant::now();
    for _ in 0..INCOMING_QUEUE_CAPACITY {
        input_sender
            .try_send(TerminalInput {
                runtime_event: RuntimeEvent::Quit,
                read_at,
            })
            .expect("an input event inside the queue limit fits");
    }
    let client_id = ClientId::new();
    let resized_viewport_size = Size {
        column_count: 90,
        row_count: 30,
    };
    let rejected_send_result = input_sender.try_send(TerminalInput {
        runtime_event: RuntimeEvent::Resize {
            client_id,
            viewport_size: resized_viewport_size,
            pane_area: None,
            cell_size: None,
        },
        read_at,
    });

    let Err(mpsc::TrySendError::Full(TerminalInput {
        runtime_event:
            RuntimeEvent::Resize {
                client_id: actual_client_id,
                viewport_size,
                pane_area,
                cell_size,
            },
        read_at: rejected_read_at,
    })) = rejected_send_result
    else {
        panic!("the input event past the queue limit was not returned as full");
    };
    assert_eq!(actual_client_id, client_id);
    assert_eq!(viewport_size, resized_viewport_size);
    assert_eq!(pane_area, None);
    assert_eq!(cell_size, None);
    assert_eq!(rejected_read_at, read_at);
    for _ in 0..INCOMING_QUEUE_CAPACITY {
        let terminal_input = input_receiver
            .try_recv()
            .expect("each accepted input remains queued");
        let RuntimeEvent::Quit = terminal_input.runtime_event else {
            panic!("the input queue contained another event");
        };
        assert_eq!(terminal_input.read_at, read_at);
    }
    assert_eq!(
        input_receiver.try_recv().err(),
        Some(mpsc::TryRecvError::Empty)
    );
}

#[test]
fn incoming_batch_defers_an_image_chunk_past_the_byte_limit() {
    let (incoming_sender, incoming_receiver) = mpsc::channel();
    let chunk_byte_count = MAX_INCOMING_IMAGE_BYTE_COUNT_PER_BATCH / 2 + 1;
    let first_chunk_frame = koshi_test_support::fixtures::build_image_chunk_frame(
        7,
        0,
        false,
        &vec![1; chunk_byte_count],
    );
    let second_chunk_frame = koshi_test_support::fixtures::build_image_chunk_frame(
        7,
        u64::try_from(chunk_byte_count).expect("the chunk length fits an offset"),
        true,
        &vec![2; chunk_byte_count],
    );
    incoming_sender
        .send(Incoming::ImageChunk {
            connection_index: 0,
            image_chunk_frame: first_chunk_frame.clone(),
        })
        .expect("the first image chunk is queued");
    incoming_sender
        .send(Incoming::ImageChunk {
            connection_index: 0,
            image_chunk_frame: second_chunk_frame.clone(),
        })
        .expect("the second image chunk is queued");
    let first_incoming_frame = incoming_receiver
        .try_recv()
        .expect("the first image chunk is available");

    let (incoming_events, deferred_incoming_event) =
        build_incoming_batch(Some(first_incoming_frame), &incoming_receiver);

    assert_eq!(incoming_events.len(), 1);
    let mut incoming_event_iterator = incoming_events.into_iter();
    let Some(Incoming::ImageChunk {
        connection_index,
        image_chunk_frame,
    }) = incoming_event_iterator.next()
    else {
        panic!("the batch did not contain the first image chunk");
    };
    assert_eq!(connection_index, 0);
    assert_eq!(image_chunk_frame, first_chunk_frame);
    assert_eq!(incoming_event_iterator.next().map(|_| ()), None);
    let Some(Incoming::ImageChunk {
        connection_index,
        image_chunk_frame,
    }) = deferred_incoming_event
    else {
        panic!("the second image chunk was not deferred");
    };
    assert_eq!(connection_index, 0);
    assert_eq!(image_chunk_frame, second_chunk_frame);
    assert_eq!(
        incoming_receiver.try_recv().err(),
        Some(mpsc::TryRecvError::Empty)
    );
}

#[test]
fn a_new_connection_is_told_the_size_the_terminal_is_now() {
    let mut client = build_test_client();
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);

    report_terminal_size(&mut client, &mut uplink);

    let request = request_receiver.try_recv().expect("the size was reported");
    assert_eq!(request.request_id, FIRST_POST_ATTACH_REQUEST_ID);
    let IpcRequestKind::Resize {
        viewport_size,
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
        request_receiver.try_recv().err(),
        Some(mpsc::TryRecvError::Empty),
        "the size is reported once"
    );
}

#[test]
fn a_new_connection_reports_a_measured_cell_size() {
    let mut client = build_test_client();
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let measured_cell_size = koshi_core::geometry::PixelCellSize::from_pixel_dimensions(10, 20)
        .expect("positive cell dimensions");
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, true, false);

    report_terminal_size_with_cell_size(
        &mut client,
        &mut uplink,
        &mut cell_size_query,
        Some(measured_cell_size),
    );

    let request = request_receiver.try_recv().expect("the size was reported");
    let IpcRequestKind::Resize { cell_size, .. } = request.request_kind else {
        panic!("expected a Resize, got {:?}", request.request_kind);
    };
    assert_eq!(cell_size, Some(measured_cell_size));
}

#[test]
fn the_redial_wait_doubles_to_eight_seconds_and_holds_there() {
    let initial_redial_wait = FIRST_REDIAL_WAIT_DURATION;
    let doubled_redial_wait = compute_next_redial_wait(initial_redial_wait);
    let quadrupled_redial_wait = compute_next_redial_wait(doubled_redial_wait);
    let capped_redial_wait = compute_next_redial_wait(quadrupled_redial_wait);
    let repeated_capped_redial_wait = compute_next_redial_wait(capped_redial_wait);

    assert_eq!(initial_redial_wait, Duration::from_secs(1));
    assert_eq!(doubled_redial_wait, Duration::from_secs(2));
    assert_eq!(quadrupled_redial_wait, Duration::from_secs(4));
    assert_eq!(capped_redial_wait, Duration::from_secs(8));
    assert_eq!(repeated_capped_redial_wait, Duration::from_secs(8));
}

#[test]
fn a_pause_ending_on_or_past_the_window_is_not_taken() {
    // The first pause always fits: the ladder dials at least once.
    assert!(can_redial_pause_fit(
        Duration::ZERO,
        FIRST_REDIAL_WAIT_DURATION
    ));

    // 111 + 8 lands at 119 and fits; 112 + 8 lands exactly on 120 and does not.
    assert!(can_redial_pause_fit(
        Duration::from_secs(111),
        Duration::from_secs(8)
    ));
    assert!(!can_redial_pause_fit(
        Duration::from_secs(112),
        Duration::from_secs(8)
    ));

    // A pause that would end long past the window is refused too.
    assert!(!can_redial_pause_fit(
        REDIAL_WINDOW_DURATION,
        FIRST_REDIAL_WAIT_DURATION
    ));
}

#[test]
fn every_pause_the_ladder_hands_out_stays_inside_the_window() {
    // Walking the real ladder from the first pause, the loop stops before any
    // pause crosses 120 seconds, and it stops after taking at least one.
    let mut elapsed_duration = Duration::ZERO;
    let mut redial_wait = FIRST_REDIAL_WAIT_DURATION;
    let mut pause_count = 0;
    while can_redial_pause_fit(elapsed_duration, redial_wait) {
        elapsed_duration += redial_wait;
        redial_wait = compute_next_redial_wait(redial_wait);
        pause_count += 1;
    }

    assert_eq!(pause_count, 17);
    assert_eq!(elapsed_duration, Duration::from_secs(119));
    assert!(elapsed_duration < REDIAL_WINDOW_DURATION);
}

#[test]
fn a_pass_that_moves_nothing_leaves_what_the_viewer_paints_alone() {
    // The repaint fires on a difference, so two reads of one unchanged viewer
    // are equal.
    let client = build_test_client();
    let render_snapshot = build_render_snapshot(&build_received_painted_frame(
        build_test_painted_frame_with_lock_mode(LockMode::Normal),
    ));
    let active_tab_id = render_snapshot.client_snapshot.active_tab_id;

    assert_eq!(
        ViewerPaint::from_client(&client, active_tab_id, &render_snapshot),
        ViewerPaint::from_client(&client, active_tab_id, &render_snapshot)
    );
}

/// How long a test waits for the reader thread to forward a frame before it
/// fails.
const FRAME_READ_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

/// A live event stream: the session end that writes frames, and both halves of
/// the client end. The frame-reader thread owns the reading half, and the
/// caller holds the writing half.
///
/// The accept runs on its own thread while this thread connects.
fn build_test_event_stream() -> (Connection, FrameReader, FrameWriter) {
    // On Unix the address is a socket-file path under `/tmp`. On Windows it is
    // a pipe name, and the directory is unused. A new session id makes each
    // address unique.
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
    // The reader steps over a frame this build has no name for and forwards the
    // frame after it.
    let (mut session_connection, frame_reader, _frame_writer) = build_test_event_stream();
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    spawn_frame_reader(frame_reader, 4, incoming_sender);

    let tab_closed_event = SessionEvent::TabClosed {
        tab_id: TabId::new(),
    };
    session_connection
        .send(&serde_json::json!({ "Floating": { "pane_id": 1 } }))
        .expect("write a frame this build has no name for");
    session_connection
        .send(&tab_closed_event)
        .expect("write the frame after it, which this build has");

    let Incoming::Frame {
        connection_index,
        session_event_result,
    } = incoming_receiver
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
        tab_closed_event,
        "the frame after the one this build cannot name is the first the loop is handed",
    );
}

#[test]
fn the_reader_applies_a_painted_update_and_ends_at_a_binary_frame_that_does_not_read() {
    // A binary frame of kind 9 is stepped over. The parts of a painted update
    // reach the loop as one applied update, the JSON frame after them arrives,
    // and a binary frame with no kind byte is a failed read that ends the
    // reader.
    let (session_connection, frame_reader, _frame_writer) = build_test_event_stream();
    let (_session_reader, mut session_writer) = session_connection.split();
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    spawn_frame_reader(frame_reader, 4, incoming_sender);
    let painted_frame = build_test_painted_frame();
    let render_snapshot =
        build_render_snapshot(&build_received_painted_frame(painted_frame.clone()));
    let part_payloads = PaintedUpdateSender::new()
        .build_update_frames(&render_snapshot, &painted_frame, false)
        .expect("the painted update fits the frame cap")
        .expect("the first update of a connection is sent");
    let tab_closed_event = SessionEvent::TabClosed {
        tab_id: TabId::new(),
    };

    session_writer
        .send_binary(&[9, 0, 0])
        .expect("write a binary frame of a kind this build does not have");
    for part_payload in &part_payloads {
        session_writer
            .send_binary(part_payload)
            .expect("write a part of the painted update");
    }
    session_writer
        .send(&tab_closed_event)
        .expect("write the JSON frame after the update");
    session_writer
        .send_binary(&[])
        .expect("write a binary frame with no kind byte");

    let Incoming::Painted {
        connection_index,
        received_painted_frame,
    } = incoming_receiver
        .recv_timeout(FRAME_READ_TIMEOUT_DURATION)
        .expect("the reader forwarded the update")
    else {
        panic!("the first frame the loop is handed is the painted update");
    };
    assert_eq!(connection_index, 4);
    assert_eq!(
        *received_painted_frame,
        build_received_painted_frame(painted_frame)
    );
    let Incoming::Frame {
        connection_index,
        session_event_result,
    } = incoming_receiver
        .recv_timeout(FRAME_READ_TIMEOUT_DURATION)
        .expect("the reader forwarded the JSON frame")
    else {
        panic!("the second frame the loop is handed is the JSON frame");
    };
    assert_eq!(connection_index, 4);
    assert_eq!(
        session_event_result.expect("the JSON frame decoded"),
        tab_closed_event
    );
    let Incoming::Frame {
        connection_index,
        session_event_result: Err(IpcError::MalformedFrame { error_detail }),
    } = incoming_receiver
        .recv_timeout(FRAME_READ_TIMEOUT_DURATION)
        .expect("the reader forwarded the failed read")
    else {
        panic!("the third frame the loop is handed is the failed read");
    };
    assert_eq!(connection_index, 4);
    assert_eq!(error_detail, "the binary frame holds no kind byte");
    assert_eq!(
        incoming_receiver
            .recv_timeout(FRAME_READ_TIMEOUT_DURATION)
            .err(),
        Some(mpsc::RecvTimeoutError::Disconnected),
        "the reader ends after the failed read"
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
fn a_number_too_large_to_parse_is_refused_like_any_other_line() {
    let session_rows = vec![build_test_session_row("a")];
    let selection_error = parse_session_selection(&session_rows, "99999999999999999999999999")
        .expect_err("a number past the end of `usize` names no listed row");

    assert_eq!(
        selection_error.to_string(),
        "invalid arguments: `99999999999999999999999999` is not one of the listed \
         sessions; expected a number 1 to 1"
    );
    assert_eq!(
        CliExitCode::from(&selection_error),
        CliExitCode::UsageOrConfig
    );
}

#[test]
fn spaces_around_the_typed_number_still_pick_that_row() {
    let session_rows = vec![build_test_session_row("a"), build_test_session_row("b")];

    assert_eq!(
        parse_session_selection(&session_rows, "  2  \n").expect("the trimmed line names row 2"),
        1
    );
}

/// The redraw interval of a placement animation at `max-frames-per-second 60`.
const FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND: Duration = Duration::from_nanos(16_666_667);

/// Two frames of one tab. The first has `left_pane_id` at columns `0..39` and
/// `right_pane_id` at columns `41..80`. The second has the two panes swapped.
fn build_swapped_pane_frames(
    left_pane_id: PaneId,
    right_pane_id: PaneId,
) -> (PaintedFrame, PaintedFrame) {
    let left_outer_rect = Rect::from_origin_and_size(
        Point { column: 0, row: 0 },
        Size {
            column_count: 39,
            row_count: 22,
        },
    );
    let right_outer_rect = Rect::from_origin_and_size(
        Point { column: 41, row: 0 },
        Size {
            column_count: 39,
            row_count: 22,
        },
    );
    let build_frame_slot = |pane_id, outer_rect: Rect| FrameSlot {
        pane_id,
        outer_rect,
        content_rect: Some(outer_rect.compute_inner_with_border()),
        is_visible: true,
        is_suppressed: false,
    };
    let mut initial_frame = build_test_painted_frame();
    initial_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots = vec![
        build_frame_slot(left_pane_id, left_outer_rect),
        build_frame_slot(right_pane_id, right_outer_rect),
    ];
    let mut committed_frame = initial_frame.clone();
    committed_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots = vec![
        build_frame_slot(left_pane_id, right_outer_rect),
        build_frame_slot(right_pane_id, left_outer_rect),
    ];
    (initial_frame, committed_frame)
}

/// Paint the first of [`build_swapped_pane_frames`] at `started_at`. Returns the
/// tab id, the first frame's active tab, and the committed frame.
fn paint_initial_swapped_pane_frame(
    client: &mut Client,
    screen: &mut Screen<TestBackend>,
    started_at: Instant,
) -> (TabId, TabSnapshot, RenderSnapshot) {
    let (initial_frame, committed_frame) = build_swapped_pane_frames(PaneId::new(), PaneId::new());
    let initial_render_snapshot =
        build_render_snapshot(&build_received_painted_frame(initial_frame.clone()));
    screen.pending_snapshot = Some(initial_render_snapshot.clone());
    screen
        .commit_pending_snapshot(client, started_at)
        .expect("the first frame paints");
    (
        initial_frame.client_snapshot.active_tab_id,
        initial_render_snapshot.session_snapshot.active_tab_snapshot,
        build_render_snapshot(&build_received_painted_frame(committed_frame.clone())),
    )
}

/// Return the outer and content rects of each pane `screen` drew last, in slot order.
fn list_shown_pane_rects<B: Backend>(screen: &Screen<B>) -> Vec<(Rect, Option<Rect>)> {
    screen
        .shown_tab_snapshot
        .as_ref()
        .expect("the screen drew a tab")
        .pane_slots
        .iter()
        .map(|pane_slot| (pane_slot.outer_rect, pane_slot.content_rect))
        .collect()
}

#[test]
fn another_viewers_accepted_swap_slides_both_panes_to_their_new_rects() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, initial_tab_snapshot, committed_snapshot) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    let committed_tab_snapshot = committed_snapshot
        .session_snapshot
        .active_tab_snapshot
        .clone();

    screen.note_committed_placement([Some(active_tab_id), Some(active_tab_id)]);
    assert_eq!(
        screen.refresh_at(&mut client, Some(active_tab_id), started_at),
        None,
        "the notice waits for the session frame that carries the placement"
    );
    screen.pending_snapshot = Some(committed_snapshot);
    let committed_mouse_frame = screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the committed frame paints");
    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&initial_tab_snapshot),
        "the slide starts from the rects already on screen"
    );
    assert_eq!(
        committed_mouse_frame.session_snapshot.active_tab_snapshot, committed_tab_snapshot,
        "mouse input is placed against the committed rects during the slide"
    );

    let halfway_time = started_at + Duration::from_millis(80);
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            halfway_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        Some(FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND)
    );
    let halfway_mouse_frame = screen
        .refresh_at(&mut client, Some(active_tab_id), halfway_time)
        .expect("the slide repaints without a session frame");
    assert_eq!(
        halfway_mouse_frame.session_snapshot.active_tab_snapshot,
        committed_tab_snapshot
    );
    let pane_size = Size {
        column_count: 39,
        row_count: 22,
    };
    let content_size = Size {
        column_count: 37,
        row_count: 20,
    };
    assert_eq!(
        list_shown_pane_rects(&screen),
        vec![
            (
                Rect::from_origin_and_size(Point { column: 36, row: 0 }, pane_size),
                Some(Rect::from_origin_and_size(
                    Point { column: 37, row: 1 },
                    content_size
                )),
            ),
            (
                Rect::from_origin_and_size(Point { column: 5, row: 0 }, pane_size),
                Some(Rect::from_origin_and_size(
                    Point { column: 6, row: 1 },
                    content_size
                )),
            ),
        ],
        "80 ms into 160 ms the ease-out has covered 0.875 of each 41-column move"
    );

    let end_time = started_at + PLACEMENT_ANIMATION_DURATION;
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            end_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        Some(Duration::ZERO)
    );
    screen
        .refresh_at(&mut client, Some(active_tab_id), end_time)
        .expect("the slide's last repaint draws the committed rects");
    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&committed_tab_snapshot)
    );
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            end_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
    assert_eq!(
        screen.refresh_at(
            &mut client,
            Some(active_tab_id),
            end_time + FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None,
        "a finished slide draws nothing more"
    );
}

#[test]
fn a_committed_frame_without_a_placement_notice_draws_the_new_rects_at_once() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (_, _, committed_snapshot) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    let committed_tab_snapshot = committed_snapshot
        .session_snapshot
        .active_tab_snapshot
        .clone();

    screen.pending_snapshot = Some(committed_snapshot);
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the committed frame paints");

    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&committed_tab_snapshot)
    );
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            started_at,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
}

#[test]
fn a_placement_notice_for_another_tab_draws_the_new_rects_at_once() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (_, _, committed_snapshot) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    let committed_tab_snapshot = committed_snapshot
        .session_snapshot
        .active_tab_snapshot
        .clone();

    screen.note_committed_placement([Some(TabId::new()), Some(TabId::new())]);
    screen.pending_snapshot = Some(committed_snapshot);
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the committed frame paints");

    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&committed_tab_snapshot)
    );
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            started_at,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
}

#[test]
fn reduced_motion_draws_another_viewers_accepted_placement_at_once() {
    let mut client = build_test_client();
    client.client_config.should_reduce_motion = true;
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, _, committed_snapshot) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    let committed_tab_snapshot = committed_snapshot
        .session_snapshot
        .active_tab_snapshot
        .clone();

    screen.note_committed_placement([Some(active_tab_id), Some(active_tab_id)]);
    screen.pending_snapshot = Some(committed_snapshot);
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the committed frame paints");

    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&committed_tab_snapshot)
    );
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            started_at,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
}

#[test]
fn a_frame_for_another_tab_during_a_committed_placement_slide_draws_that_tab_at_once() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, _, committed_snapshot) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    screen.note_committed_placement([Some(active_tab_id), Some(active_tab_id)]);
    screen.pending_snapshot = Some(committed_snapshot);
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the committed frame paints");
    let other_tab_render_snapshot =
        build_render_snapshot(&build_received_painted_frame(build_test_painted_frame()));
    let other_active_tab_snapshot = other_tab_render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .clone();

    let halfway_time = started_at + Duration::from_millis(80);
    screen.pending_snapshot = Some(other_tab_render_snapshot);
    screen
        .commit_pending_snapshot(&mut client, halfway_time)
        .expect("the other tab's frame paints");

    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&other_active_tab_snapshot)
    );
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            halfway_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
}

/// When [`run_attachment_with_committed_swap_notice`] detaches the viewer,
/// counted from the draw of the committed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewerDetachMoment {
    /// Once twice [`PLACEMENT_ANIMATION_DURATION`] has passed.
    AfterTwoSlideDurations,
    /// Once the viewer has drawn a third frame and its drawn cells equal a
    /// fresh viewer's draw of the committed frame.
    AfterCommittedLayoutDrawn,
}

/// Run an attachment that paints the swapped-pane frames with a commit notice
/// for `notice_command_id` between them. With `pending_command_id` set, the
/// client waits for that placement command. The session detaches the viewer at
/// `viewer_detach_moment`. Returns the draw count, the tab drawn last, and the
/// committed frame's tab.
fn run_attachment_with_committed_swap_notice(
    pending_command_id: Option<CommandId>,
    notice_command_id: CommandId,
    viewer_detach_moment: ViewerDetachMoment,
) -> (usize, Option<TabSnapshot>, TabSnapshot) {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let session_id = SessionId::new();
    let left_pane_id = PaneId::new();
    let right_pane_id = PaneId::new();
    let (mut initial_frame, mut committed_frame) =
        build_swapped_pane_frames(left_pane_id, right_pane_id);
    let active_tab_id = initial_frame.client_snapshot.active_tab_id;
    for painted_frame in [&mut initial_frame, &mut committed_frame] {
        painted_frame.session_snapshot.session_id = session_id;
        painted_frame.client_snapshot.client_id = client_id;
    }
    let committed_render_snapshot =
        build_render_snapshot(&build_received_painted_frame(committed_frame.clone()));
    let committed_layout_buffer = build_reference_drawn_buffer(&committed_render_snapshot);
    let committed_tab_snapshot = committed_render_snapshot
        .session_snapshot
        .active_tab_snapshot;
    client.set_session_id(session_id);
    if let Some(pending_command_id) = pending_command_id {
        client.placement_state.placement_mode = Some(PlacementMode {
            source_pane_id: left_pane_id,
            source_tab_id: Some(active_tab_id),
            destination_tab_id: active_tab_id,
            placement_direction: Direction::Right,
            placement_target: Some(TiledPlacementTarget::Swap {
                target_pane_id: right_pane_id,
            }),
            pending_placement_command_id: Some(pending_command_id),
        });
    }

    let draw_count = Arc::new(AtomicUsize::new(0));
    let failing_backend = FailingBackend::from_terminal_size_with_draw_count(
        TEST_VIEWPORT_SIZE,
        Arc::clone(&draw_count),
    );
    let drawn_buffer = failing_backend.share_drawn_buffer();
    let mut screen = Screen::from_backend_and_viewport_size(failing_backend, TEST_VIEWPORT_SIZE);
    let (request_sender, _request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    let producer_sender = incoming_sender.clone();
    let producer_draw_count = Arc::clone(&draw_count);
    let producer_thread = thread::spawn(move || -> Result<(), String> {
        let send_session_event = |session_event: SessionEvent| {
            producer_sender
                .send(build_session_event_incoming(session_event))
                .map_err(|send_error| send_error.to_string())
        };
        producer_sender
            .send(Incoming::Painted {
                connection_index: INITIAL_CONNECTION_INDEX,
                received_painted_frame: Box::new(build_received_painted_frame(initial_frame)),
            })
            .map_err(|send_error| send_error.to_string())?;
        if !wait_for_draw_count(&producer_draw_count, 1) {
            let _ = producer_sender.send(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
            return Err(String::from("the first frame was not drawn"));
        }
        send_session_event(SessionEvent::PanePlacementCommitted {
            command_id: notice_command_id,
            source_pane_id: left_pane_id,
            source_tab_id: Some(active_tab_id),
            destination_tab_id: Some(active_tab_id),
            placement_target: PanePlacementTarget::Tiled(TiledPlacementTarget::Swap {
                target_pane_id: right_pane_id,
            }),
        })?;
        producer_sender
            .send(Incoming::Painted {
                connection_index: INITIAL_CONNECTION_INDEX,
                received_painted_frame: Box::new(build_received_painted_frame(committed_frame)),
            })
            .map_err(|send_error| send_error.to_string())?;
        if !wait_for_draw_count(&producer_draw_count, 2) {
            let _ = producer_sender.send(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
            return Err(String::from("the committed frame was not drawn"));
        }
        match viewer_detach_moment {
            ViewerDetachMoment::AfterTwoSlideDurations => {
                thread::sleep(PLACEMENT_ANIMATION_DURATION * 2);
            }
            ViewerDetachMoment::AfterCommittedLayoutDrawn => {
                if !wait_for_drawn_buffer(
                    &producer_draw_count,
                    3,
                    &drawn_buffer,
                    &committed_layout_buffer,
                ) {
                    let _ = producer_sender
                        .send(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
                    return Err(String::from(
                        "the slide did not end on the committed layout",
                    ));
                }
            }
        }
        send_session_event(SessionEvent::Detached)
    });
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    let attachment_ending = run_attachment(
        &build_local_home(),
        session_id,
        client_id,
        ConnectionToken::generate(),
        None,
        &mut client,
        &mut screen,
        &mut uplink,
        TEXT_ONLY_ATTACH_SETTINGS,
        &mut cell_size_query,
        &mut Vec::new(),
        incoming_sender,
        incoming_receiver,
    );
    producer_thread
        .join()
        .expect("the frame producer finished")
        .expect("the attachment drew both frames");
    assert_eq!(attachment_ending, AttachmentEnding::Detached);
    (
        draw_count.load(Ordering::Relaxed),
        screen.shown_tab_snapshot.clone(),
        committed_tab_snapshot,
    )
}

#[test]
fn an_attached_viewer_slides_a_placement_another_viewer_committed() {
    let (draw_count, shown_tab_snapshot, committed_tab_snapshot) =
        run_attachment_with_committed_swap_notice(
            None,
            CommandId::new(),
            ViewerDetachMoment::AfterCommittedLayoutDrawn,
        );

    assert!(
        draw_count >= 3,
        "the first frame, the committed frame at the slide's start, and the slide \
         frames its timer draws with no new session event; got {draw_count}"
    );
    assert_eq!(shown_tab_snapshot, Some(committed_tab_snapshot));
}

#[test]
fn an_attached_viewer_draws_its_own_committed_placement_without_a_second_slide() {
    let command_id = CommandId::new();
    let (draw_count, shown_tab_snapshot, committed_tab_snapshot) =
        run_attachment_with_committed_swap_notice(
            Some(command_id),
            command_id,
            ViewerDetachMoment::AfterTwoSlideDurations,
        );

    assert_eq!(
        draw_count, 2,
        "only the first frame and the committed frame draw"
    );
    assert_eq!(shown_tab_snapshot, Some(committed_tab_snapshot));
}

#[test]
fn a_connection_reset_ends_a_committed_placement_slide_and_drops_waiting_notices() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, _, committed_snapshot) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    let committed_tab_snapshot = committed_snapshot
        .session_snapshot
        .active_tab_snapshot
        .clone();
    screen.note_committed_placement([Some(active_tab_id), Some(active_tab_id)]);
    screen.pending_snapshot = Some(committed_snapshot);
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the committed frame paints");
    screen.note_committed_placement([Some(active_tab_id), Some(active_tab_id)]);

    screen.reset_connection();

    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            started_at,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
    screen.pending_snapshot = screen.last_snapshot.clone();
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the committed frame paints again");
    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&committed_tab_snapshot),
        "a notice from before the reset starts no slide"
    );
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            started_at,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
}

#[test]
fn a_placement_notice_whose_frame_keeps_every_rect_starts_no_slide() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, initial_tab_snapshot, _) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);

    screen.note_committed_placement([Some(active_tab_id), Some(active_tab_id)]);
    screen.pending_snapshot = screen.last_snapshot.clone();
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the unchanged frame paints");

    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&initial_tab_snapshot)
    );
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            started_at,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None,
        "a frame with the rects already on screen schedules no slide repaint"
    );
}

/// `tab_snapshot` as a placement preview's tab: the tree puts its panes side
/// by side in slot order, a one-pane tab is that pane alone, and no pane is in
/// the focus history.
fn build_frame_placement_tab_snapshot(tab_snapshot: TabSnapshot) -> PlacementTabSnapshot {
    let pane_ids = tab_snapshot
        .pane_slots
        .iter()
        .map(|pane_slot| pane_slot.pane_id)
        .collect::<Vec<_>>();
    let layout_tree = match pane_ids.as_slice() {
        [pane_id] => LayoutNode::Pane(*pane_id),
        _ => LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            pane_ids.into_iter().map(LayoutNode::Pane).collect(),
        )),
    };
    PlacementTabSnapshot {
        layout_tree,
        focus_history: Vec::new(),
        tab_snapshot,
    }
}

/// Put `client` in pane placement mode for `source_pane_id` on the active tab
/// of `frame_snapshot`, until Esc and with no target, holding an available
/// preview that shows that tab as the frame draws it
/// ([`build_frame_placement_tab_snapshot`]). Returns the held preview.
fn hold_same_tab_placement_preview(
    client: &mut Client,
    frame_snapshot: &RenderSnapshot,
    source_pane_id: PaneId,
) -> Arc<PlacementPreview> {
    let tab_snapshot = frame_snapshot.session_snapshot.active_tab_snapshot.clone();
    let active_tab_id = tab_snapshot.tab_id;
    let tab_size = tab_snapshot.tab_size;
    let placement_preview = Arc::new(PlacementPreview::Available(Box::new(PlacementSnapshot {
        source_pane_id,
        source: PlacementSource::Tiled(build_frame_placement_tab_snapshot(tab_snapshot)),
        destination_tab_id: active_tab_id,
        destination_tab_snapshot: None,
        pane_sizing: koshi_layout::solver::PaneSizing::default(),
        insertion_tab_sizes: koshi_layout::placement::PlacementTabSizes {
            destination_tab_size: Some(tab_size),
            source_tab_size: None,
        },
        swap_tab_sizes: Vec::new(),
    })));
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
    client.placement_state.placement_preview = Some(Arc::clone(&placement_preview));
    placement_preview
}

/// The tab `placement_preview` shows as it is.
///
/// # Panics
/// When the preview is unavailable or shows no tab.
fn get_displayed_tab_snapshot(placement_preview: &PlacementPreview) -> &PlacementTabSnapshot {
    placement_preview
        .find_placement_snapshot()
        .and_then(PlacementSnapshot::find_displayed_tab_snapshot)
        .expect("the preview shows a tab")
}

/// Paint the initial swapped-pane frame at `started_at`, then put `client` in
/// dock/undock mode with a new floating pane selected over the active tab and
/// no target, holding that pane's available preview of the active tab as the
/// frame draws it. Returns the active tab id, the first tiled pane's id, and
/// the held preview.
fn select_idle_floating_dock_source(
    client: &mut Client,
    screen: &mut Screen<TestBackend>,
    started_at: Instant,
) -> (TabId, PaneId, Arc<PlacementPreview>) {
    let (active_tab_id, initial_tab_snapshot, _) =
        paint_initial_swapped_pane_frame(client, screen, started_at);
    let floating_pane_id = PaneId::new();
    let target_pane_id = initial_tab_snapshot.pane_slots[0].pane_id;
    let tab_size = initial_tab_snapshot.tab_size;
    let placement_preview = Arc::new(PlacementPreview::Available(Box::new(PlacementSnapshot {
        source_pane_id: floating_pane_id,
        source: PlacementSource::Floating,
        destination_tab_id: active_tab_id,
        destination_tab_snapshot: Some(build_frame_placement_tab_snapshot(initial_tab_snapshot)),
        pane_sizing: koshi_layout::solver::PaneSizing::default(),
        insertion_tab_sizes: koshi_layout::placement::PlacementTabSizes {
            destination_tab_size: Some(tab_size),
            source_tab_size: None,
        },
        swap_tab_sizes: Vec::new(),
    })));
    client.placement_state.placement_preview = Some(Arc::clone(&placement_preview));
    client.dock_undock_mode = Some(DockUndockMode {
        tiled_pane_selection: None,
    });
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: floating_pane_id,
        source_tab_id: None,
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    client.placement_state.placement_mode_lifetime = PlacementModeLifetime::UntilCancelled;
    (active_tab_id, target_pane_id, placement_preview)
}

#[test]
fn a_floating_dock_selection_paints_no_placement_preview_until_it_selects_a_target() {
    let mut client = build_test_client();
    client.client_config.should_reduce_motion = true;
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, target_pane_id, placement_preview) =
        select_idle_floating_dock_source(&mut client, &mut screen, started_at);
    assert!(client.is_floating_dock_selection_idle());

    screen.refresh_at(&mut client, Some(active_tab_id), started_at);
    assert_eq!(screen.shown_placement_display, None);

    let placement_target = TiledPlacementTarget::Split {
        destination_tab_id: active_tab_id,
        anchor: PanePlacementAnchor::Pane(target_pane_id),
        direction: Direction::Right,
    };
    client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on")
        .placement_target = Some(placement_target.clone());
    screen.refresh_at(&mut client, Some(active_tab_id), started_at);
    let placed_tab_snapshot = placement_preview
        .find_placement_snapshot()
        .and_then(|placement_snapshot| {
            placement_snapshot
                .build_placed_preview(&placement_target)
                .ok()
        })
        .expect("the tab takes the floating pane")
        .placed_tab_snapshot;
    assert_eq!(
        screen.shown_placement_display,
        Some(PlacementDisplay {
            placement_preview,
            displayed_tab_snapshot: placed_tab_snapshot,
        }),
        "the selected target draws the tab with the floating pane placed"
    );
}

#[test]
fn a_dock_target_with_no_room_paints_the_tab_as_it_is_and_names_the_refusal() {
    let mut client = build_test_client();
    client.client_config.should_reduce_motion = true;
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, target_pane_id, placement_preview) =
        select_idle_floating_dock_source(&mut client, &mut screen, started_at);
    let floating_pane_id = client
        .get_placement_source_pane_id()
        .expect("the floating pane is the source");
    screen
        .last_snapshot
        .as_mut()
        .expect("the first frame painted")
        .session_snapshot
        .tabs_metadata = vec![TabMetadata {
        tab_id: active_tab_id,
        tab_name: "main".to_string(),
        tab_index: 0,
        is_active: true,
    }];
    let mut placement_snapshot = placement_preview
        .find_placement_snapshot()
        .expect("the preview is available")
        .clone();
    placement_snapshot.insertion_tab_sizes.destination_tab_size = Some(NO_ROOM_TAB_SIZE);
    let refused_placement_preview =
        Arc::new(PlacementPreview::Available(Box::new(placement_snapshot)));
    client.placement_state.placement_preview = Some(Arc::clone(&refused_placement_preview));
    client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on")
        .placement_target = Some(TiledPlacementTarget::Split {
        destination_tab_id: active_tab_id,
        anchor: PanePlacementAnchor::Pane(target_pane_id),
        direction: Direction::Right,
    });

    let expected_placement_display = PlacementDisplay {
        placement_preview: Arc::clone(&refused_placement_preview),
        displayed_tab_snapshot: get_displayed_tab_snapshot(&refused_placement_preview).clone(),
    };
    let expected_placement_presentation = PanePlacementPresentation {
        source_pane_id: floating_pane_id,
        target_pane_ids: vec![target_pane_id],
        source_message: PanePlacementMessage {
            full_text: "Moving pane cannot go there".to_string(),
            compact_text: "Moving pane",
            detail_text: Some("Choose another place · Esc to cancel".to_string()),
        },
        target_message: Some(PanePlacementMessage {
            full_text: "No room for this pane in main".to_string(),
            compact_text: "No room",
            detail_text: Some("Choose another place · Esc to cancel".to_string()),
        }),
    };
    let find_shown_placement_presentation = |screen: &Screen<TestBackend>| {
        screen
            .shown_viewer_paint
            .as_ref()
            .and_then(|viewer_paint| viewer_paint.placement_presentation.clone())
    };

    screen.refresh_at(&mut client, Some(active_tab_id), started_at);
    assert_eq!(
        screen.shown_placement_display,
        Some(expected_placement_display.clone()),
        "a refused target draws the tab as it is"
    );
    assert_eq!(
        find_shown_placement_presentation(&screen),
        Some(expected_placement_presentation.clone())
    );

    // A new frame that carries the same preview keeps the refused target.
    let mut next_render_snapshot = screen
        .last_snapshot
        .clone()
        .expect("the first frame painted");
    next_render_snapshot.placement_preview = Some(Arc::clone(&refused_placement_preview));
    screen.pending_snapshot = Some(next_render_snapshot);
    screen.refresh_at(&mut client, Some(active_tab_id), started_at);
    assert_eq!(screen.pending_snapshot, None, "the new frame is painted");
    assert_eq!(
        screen.shown_placement_display,
        Some(expected_placement_display)
    );
    assert_eq!(
        find_shown_placement_presentation(&screen),
        Some(expected_placement_presentation)
    );
}

#[test]
fn an_idle_floating_dock_selection_ends_a_running_placement_animation() {
    let mut client = build_test_client();
    client.client_config.should_reduce_motion = false;
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (_, _, placement_preview) =
        select_idle_floating_dock_source(&mut client, &mut screen, started_at);
    let destination_tab_snapshot = get_displayed_tab_snapshot(&placement_preview).clone();
    screen.placement_view = PlacementView::build(Arc::clone(&placement_preview), None);
    screen.placement_animation = Some(PlacementAnimation {
        from_tab_snapshot: destination_tab_snapshot.clone(),
        to_tab_snapshot: destination_tab_snapshot,
        started_at,
    });
    assert!(client.is_floating_dock_selection_idle());

    assert_eq!(
        screen.update_previewed_placement_display(&mut client, started_at),
        None
    );
    assert_eq!(screen.placement_animation, None);
    assert_eq!(screen.placement_view, None);
}

#[test]
fn a_new_preview_animation_starts_from_the_last_visible_layout() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (_, initial_tab_snapshot, _) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    let initial_render_snapshot = screen.last_snapshot.clone().expect("initial frame");
    let source_pane_id = initial_tab_snapshot.pane_slots[0].pane_id;
    let placement_preview =
        hold_same_tab_placement_preview(&mut client, &initial_render_snapshot, source_pane_id);
    let base_tab_snapshot = get_displayed_tab_snapshot(&placement_preview).clone();
    let mut shown_tab_snapshot = base_tab_snapshot.clone();
    shown_tab_snapshot.tab_snapshot.pane_slots[0]
        .outer_rect
        .origin
        .column = 12;
    screen.shown_placement_display = Some(PlacementDisplay {
        placement_preview: Arc::clone(&placement_preview),
        displayed_tab_snapshot: shown_tab_snapshot.clone(),
    });

    let placement_display = screen.update_placement_display(&client, false, started_at);

    assert_eq!(
        placement_display,
        Some(PlacementDisplay {
            placement_preview,
            displayed_tab_snapshot: shown_tab_snapshot.clone(),
        })
    );
    assert_eq!(
        screen.placement_animation,
        Some(PlacementAnimation {
            from_tab_snapshot: shown_tab_snapshot,
            to_tab_snapshot: base_tab_snapshot,
            started_at,
        })
    );
}

#[test]
fn a_placement_preview_ends_a_committed_placement_slide() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, _, committed_snapshot) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    let committed_tab_snapshot = committed_snapshot
        .session_snapshot
        .active_tab_snapshot
        .clone();
    let source_pane_id = committed_tab_snapshot.pane_slots[0].pane_id;
    screen.note_committed_placement([Some(active_tab_id), Some(active_tab_id)]);
    screen.pending_snapshot = Some(committed_snapshot.clone());
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the committed frame paints");
    hold_same_tab_placement_preview(&mut client, &committed_snapshot, source_pane_id);

    let halfway_time = started_at + Duration::from_millis(80);
    screen
        .refresh_at(&mut client, Some(active_tab_id), halfway_time)
        .expect("the placement preview paints");

    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&committed_tab_snapshot),
        "the preview draws its own tab in place of the slide"
    );
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            halfway_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
}

#[test]
fn a_session_frame_with_no_preview_drops_the_placement_preview_before_painting() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, initial_tab_snapshot, committed_snapshot) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    let initial_render_snapshot = screen.last_snapshot.clone().expect("the initial frame");
    let committed_tab_snapshot = committed_snapshot
        .session_snapshot
        .active_tab_snapshot
        .clone();
    let source_pane_id = initial_tab_snapshot.pane_slots[0].pane_id;
    hold_same_tab_placement_preview(&mut client, &initial_render_snapshot, source_pane_id);
    screen
        .refresh_at(&mut client, Some(active_tab_id), started_at)
        .expect("the placement preview paints");
    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&initial_tab_snapshot)
    );

    screen.pending_snapshot = Some(committed_snapshot);
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the committed frame paints");

    assert_eq!(client.find_held_placement_preview(), None);
    assert_eq!(screen.placement_view, None);
    assert_eq!(screen.shown_placement_display, None);
    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&committed_tab_snapshot),
        "the committed rects draw in place of the preview"
    );
}

#[test]
fn a_screen_preview_the_client_no_longer_holds_is_dropped_at_the_next_refresh() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, initial_tab_snapshot, _) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    let initial_snapshot = screen
        .last_snapshot
        .clone()
        .expect("the first frame is shown");
    let placement_preview = hold_same_tab_placement_preview(
        &mut client,
        &initial_snapshot,
        initial_tab_snapshot.pane_slots[0].pane_id,
    );
    client.cancel_placement_mode();
    screen.placement_view = PlacementView::build(placement_preview, None);
    assert_eq!(client.get_placement_snapshot(), None);

    screen.refresh_at(&mut client, Some(active_tab_id), started_at);

    assert_eq!(screen.placement_view, None);
}

#[test]
fn a_frame_with_no_preview_slides_the_preview_back_to_its_tab_then_ends_it_while_the_placement_waits(
) {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, initial_tab_snapshot, committed_snapshot) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    let initial_snapshot = screen
        .last_snapshot
        .clone()
        .expect("the first frame is shown");
    let committed_tab_snapshot = committed_snapshot
        .session_snapshot
        .active_tab_snapshot
        .clone();
    let source_pane_id = initial_tab_snapshot.pane_slots[0].pane_id;
    let target_pane_id = initial_tab_snapshot.pane_slots[1].pane_id;
    let placement_preview =
        hold_same_tab_placement_preview(&mut client, &initial_snapshot, source_pane_id);
    let placement_mode = client
        .placement_state
        .placement_mode
        .as_mut()
        .expect("pane placement mode is on");
    placement_mode.placement_target = Some(TiledPlacementTarget::Swap { target_pane_id });
    placement_mode.pending_placement_command_id = Some(CommandId::new());
    screen
        .refresh_at(&mut client, Some(active_tab_id), started_at)
        .expect("the swap target paints");

    let frame_time = started_at + Duration::from_millis(10);
    screen.pending_snapshot = Some(committed_snapshot);
    screen
        .commit_pending_snapshot(&mut client, frame_time)
        .expect("the frame with no preview paints");

    assert!(client.is_placement_confirmation_pending());
    assert_eq!(client.find_held_placement_preview(), None);
    assert_eq!(
        screen
            .placement_animation
            .as_ref()
            .map(|placement_animation| (
                &placement_animation.to_tab_snapshot,
                placement_animation.started_at
            )),
        Some((get_displayed_tab_snapshot(&placement_preview), frame_time)),
        "the preview slides back to its tab as it is"
    );

    let settled_time = frame_time + PLACEMENT_ANIMATION_DURATION;
    screen
        .refresh_at(&mut client, Some(active_tab_id), settled_time)
        .expect("the frame without the preview paints");

    assert_eq!(screen.placement_view, None);
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            settled_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&committed_tab_snapshot)
    );
}

/// Paint the swapped-pane frames with a placement notice between them, after
/// `apply_tab_change` edits the committed frame's tab, and assert that the
/// committed tab draws at once with no slide scheduled.
fn assert_committed_tab_change_starts_no_slide(
    tab_change_name: &str,
    apply_tab_change: fn(&mut TabSnapshot),
) {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let started_at = Instant::now();
    let (active_tab_id, _, mut committed_snapshot) =
        paint_initial_swapped_pane_frame(&mut client, &mut screen, started_at);
    apply_tab_change(&mut committed_snapshot.session_snapshot.active_tab_snapshot);
    let committed_tab_snapshot = committed_snapshot
        .session_snapshot
        .active_tab_snapshot
        .clone();

    screen.note_committed_placement([Some(active_tab_id), Some(active_tab_id)]);
    screen.pending_snapshot = Some(committed_snapshot);
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the committed frame paints");

    assert_eq!(
        screen.shown_tab_snapshot.as_ref(),
        Some(&committed_tab_snapshot),
        "a committed frame with a changed {tab_change_name} draws at once"
    );
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            started_at,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None,
        "a committed frame with a changed {tab_change_name} schedules no slide"
    );
}

#[test]
fn a_committed_frame_with_another_tab_size_layout_mode_or_no_room_starts_no_slide() {
    assert_committed_tab_change_starts_no_slide("tab size", |tab_snapshot| {
        tab_snapshot.tab_size = Size {
            column_count: 79,
            row_count: 24,
        };
    });
    assert_committed_tab_change_starts_no_slide("layout mode", |tab_snapshot| {
        tab_snapshot.layout_mode = LayoutMode::Fullscreen {
            focused_pane_id: tab_snapshot.pane_slots[0].pane_id,
        };
    });
    assert_committed_tab_change_starts_no_slide("every pane suppressed", |tab_snapshot| {
        tab_snapshot.is_every_pane_suppressed = true;
    });
}

#[test]
fn a_committed_placement_records_only_the_tab_ids_it_names() {
    let mut screen = build_test_screen();
    let destination_tab_id = TabId::new();

    screen.note_committed_placement([None, Some(destination_tab_id)]);
    screen.note_committed_placement([None, None]);

    assert_eq!(screen.committed_placement_tab_ids, vec![destination_tab_id]);
}

#[test]
fn an_attach_request_carries_the_link_timeout_it_is_given() {
    for link_timeout_ms in [None, Some(30_000)] {
        let attach_request = build_attach_prepare_request(
            None,
            None,
            TEXT_ONLY_ATTACH_SETTINGS,
            None,
            link_timeout_ms,
            None,
        );
        let IpcRequestKind::Attach {
            link_timeout_ms: sent_link_timeout_ms,
            ..
        } = take_prepared_attach(attach_request)
        else {
            panic!("expected an attach request");
        };
        assert_eq!(sent_link_timeout_ms, link_timeout_ms);
    }
}

/// Run one attachment in `home` with `remote_link_timeout_ms`, with
/// `first_incoming_events` queued before it starts, then after
/// `terminal_gone_delay` tell it this terminal went away. Returns how it ended
/// and every request it queued.
fn run_attachment_until_the_terminal_goes(
    home: Home,
    remote_link_timeout_ms: u32,
    first_incoming_events: Vec<Incoming>,
    terminal_gone_delay: Duration,
) -> (AttachmentEnding, Vec<IpcRequestKind>) {
    let (incoming_sender, request_receiver, attachment_ending_receiver) =
        start_test_attachment(home, remote_link_timeout_ms, first_incoming_events);
    thread::sleep(terminal_gone_delay);
    let _ = incoming_sender.send(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
    let attachment_ending = attachment_ending_receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the attachment ended");
    let sent_request_kinds: Vec<IpcRequestKind> = request_receiver
        .try_iter()
        .map(|sent_request| sent_request.request_kind)
        .collect();
    (attachment_ending, sent_request_kinds)
}

/// Start one attachment in `home` with `remote_link_timeout_ms` on its own
/// thread, with `first_incoming_events` queued in the loop's channel before
/// it starts. Returns the sender into the loop's channel, the receiver of
/// every request the loop queues, and the receiver of how the attachment
/// ended.
fn start_test_attachment(
    home: Home,
    remote_link_timeout_ms: u32,
    first_incoming_events: Vec<Incoming>,
) -> (
    mpsc::SyncSender<Incoming>,
    mpsc::Receiver<IpcRequest>,
    mpsc::Receiver<AttachmentEnding>,
) {
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    for incoming_event in first_incoming_events {
        incoming_sender
            .send(incoming_event)
            .expect("the loop's channel takes it");
    }
    let loop_incoming_sender = incoming_sender.clone();
    let (request_sender, request_receiver) = mpsc::channel();
    let (attachment_ending_sender, attachment_ending_receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut client = build_test_client();
        client.client_config.remote_link_timeout_ms = remote_link_timeout_ms;
        let client_id = client.get_client_id();
        let mut screen = build_test_screen();
        let mut uplink = build_test_uplink(request_sender);
        let mut cell_size_query =
            terminal::CellSizeQuery::from_current_measurement(None, false, false);
        let attachment_ending = run_attachment(
            &home,
            SessionId::new(),
            client_id,
            ConnectionToken::from_secret(OLD_CONNECTION_TOKEN),
            None,
            &mut client,
            &mut screen,
            &mut uplink,
            TEXT_ONLY_ATTACH_SETTINGS,
            &mut cell_size_query,
            &mut Vec::new(),
            loop_incoming_sender,
            incoming_receiver,
        );
        let _ = attachment_ending_sender.send(attachment_ending);
    });
    (
        incoming_sender,
        request_receiver,
        attachment_ending_receiver,
    )
}

#[test]
fn a_remote_link_sends_its_first_ping_as_soon_as_the_attachment_starts() {
    // A 60-second link timeout pings every 20 seconds after the first ping.
    let (incoming_sender, request_receiver, attachment_ending_receiver) =
        start_test_attachment(build_remote_home(), 60_000, Vec::new());

    let first_request = request_receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the loop queues a request inside 10 seconds");
    let _ = incoming_sender.send(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
    let attachment_ending = attachment_ending_receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the attachment ended");

    assert_eq!(
        first_request,
        IpcRequest {
            request_id: FIRST_POST_ATTACH_REQUEST_ID,
            request_kind: IpcRequestKind::Ping,
        }
    );
    assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
    let later_request_kinds: Vec<IpcRequestKind> = request_receiver
        .try_iter()
        .map(|sent_request| sent_request.request_kind)
        .collect();
    assert_eq!(
        later_request_kinds,
        vec![IpcRequestKind::Leaving {
            is_returning: false
        }]
    );
}

#[test]
fn a_remote_link_pings_on_its_cadence_and_its_last_request_says_the_viewer_does_not_return() {
    let (attachment_ending, sent_request_kinds) = run_attachment_until_the_terminal_goes(
        build_remote_home(),
        30,
        Vec::new(),
        Duration::from_millis(300),
    );

    assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
    let (last_request_kind, earlier_request_kinds) = sent_request_kinds
        .split_last()
        .expect("the loop queued requests");
    assert_eq!(
        *last_request_kind,
        IpcRequestKind::Leaving {
            is_returning: false
        }
    );
    assert!(
        !earlier_request_kinds.is_empty(),
        "a 10 ms cadence pings at least once in 300 ms"
    );
    assert!(
        earlier_request_kinds
            .iter()
            .all(|request_kind| *request_kind == IpcRequestKind::Ping),
        "every request before the last is a ping, got {earlier_request_kinds:?}"
    );
}

#[test]
fn a_local_link_sends_no_ping() {
    let (attachment_ending, sent_request_kinds) = run_attachment_until_the_terminal_goes(
        build_local_home(),
        30,
        Vec::new(),
        Duration::from_millis(300),
    );

    assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
    assert_eq!(
        sent_request_kinds,
        vec![IpcRequestKind::Leaving {
            is_returning: false
        }]
    );
}

#[test]
fn a_viewer_the_session_moves_away_says_it_does_not_return() {
    let next_session_id = SessionId::new();
    let (attachment_ending, sent_request_kinds) = run_attachment_until_the_terminal_goes(
        build_local_home(),
        30_000,
        vec![build_session_event_incoming(SessionEvent::SwitchTo {
            session_id: next_session_id,
            owner_user_id: None,
        })],
        Duration::ZERO,
    );

    assert_eq!(
        attachment_ending,
        AttachmentEnding::SwitchSession {
            session_id: next_session_id,
            owner_user_id: None,
        }
    );
    assert_eq!(
        sent_request_kinds,
        vec![IpcRequestKind::Leaving {
            is_returning: false
        }]
    );
}

#[test]
fn a_remote_viewer_leaving_a_dropped_link_to_dial_again_says_it_returns() {
    // Both events wait in the queue before the loop starts, so the loop reads
    // the dropped link before its first ping. The terminal goes away before
    // the first dial is due, so no dial runs.
    let (attachment_ending, sent_request_kinds) = run_attachment_until_the_terminal_goes(
        build_remote_home(),
        30_000,
        vec![
            Incoming::Frame {
                connection_index: INITIAL_CONNECTION_INDEX,
                session_event_result: Err(IpcError::Disconnected),
            },
            build_input_incoming(RuntimeEvent::Quit, Instant::now()),
        ],
        Duration::ZERO,
    );

    assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
    assert_eq!(
        sent_request_kinds,
        vec![IpcRequestKind::Leaving { is_returning: true }],
        "the dropped link's queue is closed: no second Leaving follows"
    );
}

/// Set the action of each of `signals` to `signal_action`, `SIG_IGN` or
/// `SIG_DFL`.
#[cfg(unix)]
pub(crate) fn set_signal_actions(signals: &[libc::c_int], signal_action: libc::sighandler_t) {
    for &signal in signals {
        // SAFETY: `SIG_IGN` and `SIG_DFL` are valid actions for every
        // termination and stop signal, and the call installs no handler
        // function.
        unsafe { libc::signal(signal, signal_action) };
    }
}

/// Wait for the quit a termination signal puts on the loop's channel.
#[cfg(unix)]
fn receive_signal_quit(incoming_receiver: &mpsc::Receiver<Incoming>) {
    let Incoming::Input(terminal_input) = incoming_receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the signal reaches the attachment channel")
    else {
        panic!("the channel contained a session event");
    };
    let RuntimeEvent::Quit = terminal_input.runtime_event else {
        panic!("the channel contained another input event");
    };
}

#[cfg(unix)]
#[test]
fn a_termination_signal_ends_the_attachment_and_is_recorded_unless_this_process_ignores_it() {
    // This is the only test that starts a watch or raises a termination
    // signal: the signals' actions belong to the whole test process.
    let (incoming_sender, incoming_receiver) = mpsc::sync_channel(INCOMING_QUEUE_CAPACITY);
    let should_run_default_action = &TERMINATION_SIGNAL_ACTIONS.should_run_default_action;

    set_signal_actions(&TERMINATION_SIGNALS, libc::SIG_IGN);
    let watch_of_ignored_signals =
        TerminalCloseWatch::start_terminal_close_watch(incoming_sender.clone())
            .expect("the signal actions are read");
    assert!(
        watch_of_ignored_signals.is_none(),
        "a process that ignores every termination signal starts no watch"
    );

    set_signal_actions(&[libc::SIGTERM, libc::SIGINT, libc::SIGQUIT], libc::SIG_DFL);
    let watch_without_hangup =
        TerminalCloseWatch::start_terminal_close_watch(incoming_sender.clone())
            .expect("the termination signals are registered")
            .expect("a process that ignores only SIGHUP starts a watch");
    assert!(!should_run_default_action.load(Ordering::SeqCst));
    signal_hook::low_level::raise(libc::SIGHUP).expect("SIGHUP is raised");
    signal_hook::low_level::raise(libc::SIGTERM).expect("SIGTERM is raised");
    receive_signal_quit(&incoming_receiver);
    assert_eq!(
        take_received_termination_signal(),
        Some(libc::SIGTERM),
        "the ignored SIGHUP is neither watched nor recorded"
    );
    assert!(
        should_run_default_action.load(Ordering::SeqCst),
        "the next signal after a taken one runs its default action"
    );
    drop(watch_without_hangup);

    set_signal_actions(&[libc::SIGHUP], libc::SIG_DFL);
    let watch_of_every_signal =
        TerminalCloseWatch::start_terminal_close_watch(incoming_sender.clone())
            .expect("the termination signals are registered")
            .expect("a process that ignores no termination signal starts a watch");
    assert!(!should_run_default_action.load(Ordering::SeqCst));
    signal_hook::low_level::raise(libc::SIGHUP).expect("SIGHUP is raised");
    receive_signal_quit(&incoming_receiver);
    assert_eq!(take_received_termination_signal(), Some(libc::SIGHUP));
    assert_eq!(take_received_termination_signal(), None);
    drop(watch_of_every_signal);

    let watch_that_takes_no_signal =
        TerminalCloseWatch::start_terminal_close_watch(incoming_sender)
            .expect("the termination signals are registered")
            .expect("a process that ignores no termination signal starts a watch");
    assert!(!should_run_default_action.load(Ordering::SeqCst));
    drop(watch_that_takes_no_signal);
    assert!(
        should_run_default_action.load(Ordering::SeqCst),
        "a stopped watch lets the default action run"
    );
    assert_eq!(
        incoming_receiver
            .recv_timeout(Duration::from_secs(10))
            .err(),
        Some(mpsc::RecvTimeoutError::Disconnected),
        "each stopped watch ends its thread and sends nothing more"
    );
}

#[test]
fn a_late_painted_frame_that_names_no_floating_pane_move_keeps_the_move_drawn() {
    let tiled_pane_id = PaneId::new();
    let floating_pane_id = PaneId::new();
    let mut mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(tiled_pane_id)]);
    mouse_frame
        .session_snapshot
        .floating_pane_slots
        .push(FloatingPaneSlot::from_outer_rect(
            floating_pane_id,
            Rect::from_origin_and_size(
                Point { column: 10, row: 4 },
                Size {
                    column_count: 40,
                    row_count: 12,
                },
            ),
            false,
        ));
    mouse_frame
        .mouse_panes
        .push(build_plain_mouse_pane(floating_pane_id));
    let mut client = build_test_client();
    let current_time = Instant::now();
    // The grab handle sits on screen row 6, columns 12 and 13.
    client.handle_mouse(
        build_mouse_press(Point { column: 12, row: 6 }),
        &mouse_frame,
        current_time,
    );
    client.handle_mouse(
        build_mouse_drag(Point { column: 17, row: 7 }),
        &mouse_frame,
        current_time + Duration::from_secs(1),
    );
    let release_actions = client.handle_mouse(
        MouseInput {
            mouse_kind: MouseKind::Release(MouseButton::Left),
            position: Point { column: 17, row: 7 },
            modifier_flags: BindingModifierFlags::NONE,
        },
        &mouse_frame,
        current_time + Duration::from_secs(2),
    );
    let [MouseAction::MoveFloatingPane { command_id, .. }] = release_actions.as_slice() else {
        panic!("expected one floating pane move, got {release_actions:?}");
    };
    let moved_draft = Some(FloatingPaneMoveDraft {
        pane_id: floating_pane_id,
        top_left_cell: Point { column: 15, row: 5 },
    });
    let mut screen = build_failing_screen();
    let frame_before_the_move = build_test_painted_frame();
    let active_tab_id = frame_before_the_move.client_snapshot.active_tab_id;
    let mut frame_after_the_move = build_test_painted_frame();
    frame_after_the_move
        .client_snapshot
        .last_applied_floating_pane_move_command_id = Some(*command_id);
    screen.paint_driver.backend.should_fail_draw = true;

    assert_eq!(
        screen.draw_painted_frame(&mut client, Box::new(frame_before_the_move)),
        None,
        "the backend rejects the frame, and the frame stays pending"
    );
    screen.paint_driver.backend.should_fail_draw = false;
    screen
        .refresh(&mut client, Some(active_tab_id))
        .expect("the pending frame paints on the next attempt");
    assert_eq!(
        client
            .build_viewer_chrome(active_tab_id)
            .floating_pane_move_draft,
        moved_draft,
        "a frame that names no move leaves the move drawn"
    );

    screen
        .draw_painted_frame(&mut client, Box::new(frame_after_the_move))
        .expect("the frame paints");
    assert_eq!(
        client
            .build_viewer_chrome(active_tab_id)
            .floating_pane_move_draft,
        None
    );
}

/// The instant `elapsed_duration` before the clock reading now.
///
/// # Panics
/// Panics when less than `elapsed_duration` passed since the clock's origin.
fn compute_instant_before_now(elapsed_duration: Duration) -> Instant {
    Instant::now()
        .checked_sub(elapsed_duration)
        .expect("the clock reads past the duration")
}

/// `<C-y>`, the chord the key-timing tests bind.
fn build_ctrl_y_chord() -> KeyChord {
    KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Char('y'))
}

/// Bind `<C-y>` to `core:new-tab` and `<C-y> a` to `core:close-tab` in
/// `client`'s normal mode, so `<C-y>` is a binding and a longer one's prefix.
fn bind_ambiguous_ctrl_y(client: &mut Client) {
    bind_core_actions_in_normal_mode(
        client,
        &[
            (KeySequence::from(build_ctrl_y_chord()), "new-tab"),
            (
                KeySequence::from_first_and_rest(
                    build_ctrl_y_chord(),
                    vec![build_plain_chord('a')],
                ),
                "close-tab",
            ),
        ],
        None,
    );
}

#[test]
fn a_key_read_after_the_ambiguity_deadline_fires_the_open_binding_and_then_resolves_alone() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    bind_ambiguous_ctrl_y(&mut client);
    let chord_timeout = client.keymap_catalog.get_chord_timeout();
    let ctrl_y_read_at = compute_instant_before_now(Duration::from_secs(2));
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);

    process_runtime_input(
        &mut client,
        &mut uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(build_ctrl_y_chord()),
        },
        ctrl_y_read_at,
    );
    process_runtime_input(
        &mut client,
        &mut uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(build_plain_chord('a')),
        },
        ctrl_y_read_at + chord_timeout + Duration::from_millis(1),
    );

    let sent_requests: Vec<IpcRequest> = request_receiver.try_iter().collect();
    assert_eq!(sent_requests.len(), 2);
    let IpcRequestKind::SubmitCommand(command_envelope) = &sent_requests[0].request_kind else {
        panic!("the expired binding goes up as its command, got {sent_requests:?}");
    };
    assert_eq!(
        command_envelope.command,
        Command::NewTab(NewTabArgs::default())
    );
    assert_eq!(
        sent_requests[1].request_kind,
        IpcRequestKind::Keyboard {
            key_input: crate::tests::build_key_input_for_chord(build_plain_chord('a')),
        },
        "`a` no longer continues the sequence and goes to the pane"
    );
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_key_read_before_the_ambiguity_deadline_continues_the_sequence_however_late_it_is_handled() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    bind_ambiguous_ctrl_y(&mut client);
    let ctrl_y_read_at = compute_instant_before_now(Duration::from_secs(2));
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);

    process_runtime_input(
        &mut client,
        &mut uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(build_ctrl_y_chord()),
        },
        ctrl_y_read_at,
    );
    process_runtime_input(
        &mut client,
        &mut uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(build_plain_chord('a')),
        },
        ctrl_y_read_at + Duration::from_millis(100),
    );

    let sent_requests: Vec<IpcRequest> = request_receiver.try_iter().collect();
    assert_eq!(sent_requests.len(), 1);
    let IpcRequestKind::SubmitCommand(command_envelope) = &sent_requests[0].request_kind else {
        panic!("the completed sequence goes up as its command, got {sent_requests:?}");
    };
    assert_eq!(
        command_envelope.command,
        Command::CloseTab(CloseTabArgs::default())
    );
}

#[test]
fn a_pass_leaves_an_expired_deadline_alone_while_a_key_read_before_it_is_on_its_way() {
    // The input thread read a resize, then a key, both inside the 100 ms
    // deadline. The key has not reached the loop when the pass that handles
    // the resize ends past the deadline.
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    bind_core_actions_in_normal_mode(
        &mut client,
        &[
            (KeySequence::from(build_ctrl_y_chord()), "quit"),
            (
                KeySequence::from_first_and_rest(
                    build_ctrl_y_chord(),
                    vec![build_plain_chord('a')],
                ),
                "new-tab",
            ),
        ],
        Some(100),
    );
    assert_eq!(
        client.resolve_key(build_ctrl_y_chord(), Instant::now()),
        KeyOutcome::Pending
    );
    let resize_read_at = client
        .get_input_read_clock()
        .stamp_input_read(Instant::now());
    client
        .get_input_read_clock()
        .stamp_input_read(Instant::now());
    thread::sleep(Duration::from_millis(150));
    let resized_viewport_size = Size {
        column_count: 100,
        row_count: 40,
    };

    let (attachment_ending, first_request_kind) = run_attachment_until_first_request(
        &mut client,
        &mut build_test_screen(),
        build_input_incoming(
            RuntimeEvent::Resize {
                client_id,
                viewport_size: resized_viewport_size,
                pane_area: None,
                cell_size: None,
            },
            resize_read_at,
        ),
    );

    assert_eq!(attachment_ending, AttachmentEnding::TerminalGone);
    assert_eq!(
        first_request_kind,
        Ok(IpcRequestKind::Resize {
            viewport_size: resized_viewport_size,
            pane_area: Some(compute_core_pane_area(resized_viewport_size)),
            cell_size: None,
        })
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from(build_ctrl_y_chord())),
        "the sequence waits for the key on its way"
    );
}

#[test]
fn while_reconnecting_a_key_read_before_the_deadline_continues_the_sequence_after_it() {
    // `<C-y>` binds `core:quit` and opens `<C-y> a b`. The input thread read
    // `a` inside the 100 ms deadline, and `a` reaches the loop after it.
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    bind_core_actions_in_normal_mode(
        &mut viewer.client,
        &[
            (KeySequence::from(build_ctrl_y_chord()), "quit"),
            (
                KeySequence::from_first_and_rest(
                    build_ctrl_y_chord(),
                    vec![build_plain_chord('a'), build_plain_chord('b')],
                ),
                "new-tab",
            ),
        ],
        Some(100),
    );
    assert_eq!(
        viewer
            .client
            .resolve_key(build_ctrl_y_chord(), Instant::now()),
        KeyOutcome::Pending
    );
    let a_key_read_at = viewer
        .client
        .get_input_read_clock()
        .stamp_input_read(Instant::now());
    thread::sleep(Duration::from_millis(150));
    viewer.queue_incoming_event(build_input_incoming(
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(build_plain_chord('a')),
        },
        a_key_read_at,
    ));
    viewer.queue_incoming_event(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(stretch_result.err(), Some(AttachmentEnding::TerminalGone));
    assert_eq!(
        viewer.client.get_pending_key_sequence(),
        Some(&KeySequence::from_first_and_rest(
            build_ctrl_y_chord(),
            vec![build_plain_chord('a')]
        ))
    );
}

#[test]
fn while_reconnecting_a_key_read_after_the_deadline_fires_the_open_binding_first() {
    // `<C-y>` binds `core:quit` and opens `<C-y> a`. The deadline passed 1.5 s
    // before the input thread read `a`.
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    bind_core_actions_in_normal_mode(
        &mut viewer.client,
        &[
            (KeySequence::from(build_ctrl_y_chord()), "quit"),
            (
                KeySequence::from_first_and_rest(
                    build_ctrl_y_chord(),
                    vec![build_plain_chord('a')],
                ),
                "new-tab",
            ),
        ],
        Some(500),
    );
    let ctrl_y_read_at = compute_instant_before_now(Duration::from_secs(2));
    assert_eq!(
        viewer
            .client
            .resolve_key(build_ctrl_y_chord(), ctrl_y_read_at),
        KeyOutcome::Pending
    );
    let a_key_read_at = viewer
        .client
        .get_input_read_clock()
        .stamp_input_read(Instant::now());
    viewer.queue_incoming_event(build_input_incoming(
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(build_plain_chord('a')),
        },
        a_key_read_at,
    ));
    viewer.queue_incoming_event(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting),
        "the quit fires before `a` is resolved"
    );
}

#[test]
fn a_key_the_loop_takes_lets_the_deadline_it_opens_wake_the_loop() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    bind_ambiguous_ctrl_y(&mut client);
    let chord_timeout = client.keymap_catalog.get_chord_timeout();
    let ctrl_y_read_at = client
        .get_input_read_clock()
        .stamp_input_read(Instant::now());
    let (request_sender, _request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);

    process_runtime_input(
        &mut client,
        &mut uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(build_ctrl_y_chord()),
        },
        ctrl_y_read_at,
    );

    assert_eq!(
        client.compute_next_key_wakeup(ctrl_y_read_at),
        Some(chord_timeout),
        "the deadline runs from the read of `<C-y>`, and no key is on its way"
    );
}

#[test]
fn while_reconnecting_a_deadline_a_taken_key_opened_fires_once_it_passes() {
    // `<C-y>` binds `core:quit` and opens `<C-y> a`. The input thread read
    // `<C-y>` 150 ms before the loop takes it, past the 100 ms deadline.
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    bind_core_actions_in_normal_mode(
        &mut viewer.client,
        &[
            (KeySequence::from(build_ctrl_y_chord()), "quit"),
            (
                KeySequence::from_first_and_rest(
                    build_ctrl_y_chord(),
                    vec![build_plain_chord('a')],
                ),
                "new-tab",
            ),
        ],
        Some(100),
    );
    let ctrl_y_read_at = viewer
        .client
        .get_input_read_clock()
        .stamp_input_read(Instant::now());
    thread::sleep(Duration::from_millis(150));
    viewer.queue_incoming_event(build_input_incoming(
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(build_ctrl_y_chord()),
        },
        ctrl_y_read_at,
    ));
    viewer.queue_incoming_event(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
    let (attempt_rejoin, _attempt_count) = build_counted_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::Redial(RedialSchedule::from_link_drop_at(Instant::now())),
        Vec::new(),
        attempt_rejoin,
    );

    assert_eq!(
        stretch_result.err(),
        Some(AttachmentEnding::LeftWhileReconnecting),
        "the quit fires before the queued quit event is read"
    );
}

#[test]
fn a_paste_read_after_the_ambiguity_deadline_goes_up_after_the_binding_the_deadline_fired() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    bind_ambiguous_ctrl_y(&mut client);
    let chord_timeout = client.keymap_catalog.get_chord_timeout();
    let ctrl_y_read_at = compute_instant_before_now(Duration::from_secs(2));
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);

    process_runtime_input(
        &mut client,
        &mut uplink,
        RuntimeEvent::KeyInput {
            client_id,
            key_input: crate::tests::build_key_input_for_chord(build_ctrl_y_chord()),
        },
        ctrl_y_read_at,
    );
    process_runtime_input(
        &mut client,
        &mut uplink,
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: String::from("typed"),
        },
        ctrl_y_read_at + chord_timeout + Duration::from_millis(1),
    );

    let sent_requests: Vec<IpcRequest> = request_receiver.try_iter().collect();
    assert_eq!(sent_requests.len(), 2);
    let IpcRequestKind::SubmitCommand(command_envelope) = &sent_requests[0].request_kind else {
        panic!("the expired binding goes up first, got {sent_requests:?}");
    };
    assert_eq!(
        command_envelope.command,
        Command::NewTab(NewTabArgs::default())
    );
    assert_eq!(
        sent_requests[1].request_kind,
        IpcRequestKind::Paste {
            pasted_text: String::from("typed"),
        }
    );
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_mouse_event_read_after_the_ambiguity_deadline_sends_the_earlier_round_then_the_binding() {
    let pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    bind_ambiguous_ctrl_y(&mut client);
    let chord_timeout = client.keymap_catalog.get_chord_timeout();
    let ctrl_y_read_at = compute_instant_before_now(Duration::from_secs(2));
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    let mut sent_border_moves = Vec::new();
    let mut pending_mouse_actions = Vec::new();
    let press_cell = get_content_cell(&mouse_frame, 0);

    // `<C-y>`, a press 50 ms after it, and a press 1 ms past the deadline, in
    // one pass.
    for (runtime_event, read_at) in [
        (
            RuntimeEvent::KeyInput {
                client_id,
                key_input: crate::tests::build_key_input_for_chord(build_ctrl_y_chord()),
            },
            ctrl_y_read_at,
        ),
        (
            RuntimeEvent::MouseInput {
                client_id,
                mouse_input: build_mouse_press(press_cell),
            },
            ctrl_y_read_at + Duration::from_millis(50),
        ),
        (
            RuntimeEvent::MouseInput {
                client_id,
                mouse_input: build_mouse_press(press_cell),
            },
            ctrl_y_read_at + chord_timeout + Duration::from_millis(1),
        ),
    ] {
        assert_eq!(
            handle_terminal_input_event(
                &mut client,
                &mut uplink,
                &mut cell_size_query,
                Some(&mouse_frame),
                TerminalInput {
                    runtime_event,
                    read_at,
                },
                &mut sent_border_moves,
                &mut pending_mouse_actions,
            ),
            ControlFlow::Continue(())
        );
    }

    let clear_selection_command =
        Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
            pane_id,
        }));
    let sent_requests: Vec<IpcRequest> = request_receiver.try_iter().collect();
    assert_eq!(sent_requests.len(), 2);
    assert_eq!(
        sent_requests[0].request_kind,
        IpcRequestKind::Mouse(vec![WireMouseAction::Command(Box::new(
            clear_selection_command.clone()
        ))]),
        "the press read before the deadline goes up before the binding"
    );
    let IpcRequestKind::SubmitCommand(command_envelope) = &sent_requests[1].request_kind else {
        panic!("the expired binding follows the round, got {sent_requests:?}");
    };
    assert_eq!(
        command_envelope.command,
        Command::NewTab(NewTabArgs::default())
    );
    assert_eq!(
        pending_mouse_actions,
        vec![MouseAction::Command(Box::new(clear_selection_command))],
        "the press read after the deadline waits for the pass's round"
    );
}

#[test]
fn an_edge_scroll_step_due_before_a_key_was_read_goes_up_before_the_key() {
    let pane_id = PaneId::new();
    let mut mouse_pane = build_plain_mouse_pane(pane_id);
    mouse_pane.view_top_row_index = 100;
    let mouse_frame = build_mouse_frame(&[mouse_pane]);
    let press_cell = get_content_cell(&mouse_frame, 0);
    let content_rect = mouse_frame.session_snapshot.active_tab_snapshot.pane_slots[0]
        .content_rect
        .expect("a visible pane");
    let below_pane_point = Point {
        column: press_cell.column,
        row: content_rect.origin.row + content_rect.size.row_count + 5,
    };
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let press_time = Instant::now();
    let drag_time = press_time + Duration::from_secs(1);
    client.handle_mouse(build_mouse_press(press_cell), &mouse_frame, press_time);
    client.handle_mouse(build_mouse_drag(below_pane_point), &mouse_frame, drag_time);
    let scroll_due_time = drag_time
        + client
            .compute_next_mouse_wakeup(drag_time)
            .expect("the pointer past the edge arms the scroll");
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);

    assert_eq!(
        handle_terminal_input_event(
            &mut client,
            &mut uplink,
            &mut cell_size_query,
            Some(&mouse_frame),
            TerminalInput {
                runtime_event: RuntimeEvent::KeyInput {
                    client_id,
                    key_input: crate::tests::build_key_input_for_chord(build_plain_chord('x')),
                },
                read_at: scroll_due_time,
            },
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        ControlFlow::Continue(())
    );

    let request_kinds: Vec<IpcRequestKind> = request_receiver
        .try_iter()
        .map(|ipc_request| ipc_request.request_kind)
        .collect();
    assert_eq!(
        request_kinds,
        vec![
            IpcRequestKind::Mouse(vec![WireMouseAction::Scroll {
                pane_id,
                is_scrolling_up: false,
                scroll_line_count: 1,
            }]),
            IpcRequestKind::Keyboard {
                key_input: crate::tests::build_key_input_for_chord(build_plain_chord('x')),
            },
        ]
    );
}

#[test]
fn a_tab_hover_that_lasted_its_delay_when_an_event_was_read_previews_its_tab() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let source_pane_id = PaneId::new();
    let source_tab_id = TabId::new();
    let destination_tab_id = TabId::new();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id,
        source_tab_id: Some(source_tab_id),
        destination_tab_id: source_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    let hover_started_at = Instant::now();
    client.update_placement_tab_hover(Some(destination_tab_id), hover_started_at);
    let (request_sender, request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let resized_viewport_size = Size {
        column_count: 100,
        row_count: 40,
    };

    process_runtime_input(
        &mut client,
        &mut uplink,
        RuntimeEvent::Resize {
            client_id,
            viewport_size: resized_viewport_size,
            pane_area: None,
            cell_size: None,
        },
        hover_started_at + crate::PLACEMENT_TAB_HOVER_DELAY_DURATION,
    );

    let request_kinds: Vec<IpcRequestKind> = request_receiver
        .try_iter()
        .map(|ipc_request| ipc_request.request_kind)
        .collect();
    assert_eq!(
        request_kinds,
        vec![IpcRequestKind::Resize {
            viewport_size: resized_viewport_size,
            pane_area: Some(compute_core_pane_area(resized_viewport_size)),
            cell_size: None,
        }]
    );
    assert_eq!(
        client.find_placement_preview_target(),
        Some(PanePlacementPreviewTarget {
            source_pane_id,
            destination_tab_id,
        })
    );
}

#[test]
fn while_reconnecting_a_paste_read_after_the_deadline_is_held_after_the_binding_it_fired() {
    let mut viewer = TestViewerWithoutLink::new();
    let client_id = viewer.client.get_client_id();
    bind_ambiguous_ctrl_y(&mut viewer.client);
    let ctrl_y_read_at = compute_instant_before_now(Duration::from_secs(2));
    assert_eq!(
        viewer
            .client
            .resolve_key(build_ctrl_y_chord(), ctrl_y_read_at),
        KeyOutcome::Pending
    );
    // The paste is stamped on the clock: the stretch judges no deadline at the
    // clock until it takes the paste.
    let paste_read_at = viewer
        .client
        .get_input_read_clock()
        .stamp_input_read(Instant::now());
    viewer.queue_incoming_event(build_input_incoming(
        RuntimeEvent::HostPaste {
            client_id,
            pasted_text: String::from("pasted"),
        },
        paste_read_at,
    ));
    let (joined_answer, _session_connection) = build_joined_answer(client_id);
    viewer.queue_incoming_event(joined_answer);
    let (attempt_rejoin, _attempt_start_receiver, _attempt_release_sender) =
        build_blocked_attempt();

    let stretch_result = viewer.run_without_link(
        LinkRecovery::RestartRejoin {
            input_hold: Some(InputHold::default()),
        },
        Vec::new(),
        attempt_rejoin,
    );

    let Ok((_joined_session, held_inputs)) = stretch_result else {
        panic!("the queued join ends the stretch");
    };
    assert_eq!(
        held_inputs,
        vec![
            HeldInput::DispatchPlan(DispatchPlan::Command(Box::new(Command::NewTab(
                NewTabArgs::default(),
            )))),
            HeldInput::Paste(String::from("pasted")),
        ]
    );
}

#[test]
fn a_tab_hover_in_placement_mode_starts_when_the_pointer_was_read_over_the_tab() {
    let pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let active_tab_id = mouse_frame.client_snapshot.active_tab_id;
    let tab_cell = (0..TEST_VIEWPORT_SIZE.column_count)
        .map(|column| Point { column, row: 0 })
        .find(|tabline_cell| {
            resolve_hit_region(mouse_frame.build_frame_layout(), *tabline_cell)
                == HitRegion::Tab {
                    tab_id: active_tab_id,
                }
        })
        .expect("the tabline draws the active tab");
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    client.placement_state.placement_mode = Some(PlacementMode {
        source_pane_id: pane_id,
        source_tab_id: Some(active_tab_id),
        destination_tab_id: active_tab_id,
        placement_direction: Direction::Right,
        placement_target: None,
        pending_placement_command_id: None,
    });
    let motion_read_at = compute_instant_before_now(Duration::from_secs(10));
    let (request_sender, _request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);

    assert_eq!(
        handle_terminal_input_event(
            &mut client,
            &mut uplink,
            &mut cell_size_query,
            Some(&mouse_frame),
            TerminalInput {
                runtime_event: RuntimeEvent::MouseInput {
                    client_id,
                    mouse_input: MouseInput {
                        mouse_kind: MouseKind::Motion,
                        position: tab_cell,
                        modifier_flags: BindingModifierFlags::NONE,
                    },
                },
                read_at: motion_read_at,
            },
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        ControlFlow::Continue(())
    );

    assert_eq!(
        client.compute_next_placement_tab_hover_wakeup(motion_read_at),
        Some(crate::PLACEMENT_TAB_HOVER_DELAY_DURATION),
        "the hover delay runs from the read of the motion"
    );
}

#[test]
fn two_presses_read_further_apart_than_a_double_click_stay_single_clicks_however_close_they_are_handled(
) {
    let pane_id = PaneId::new();
    let mouse_frame = build_mouse_frame(&[build_plain_mouse_pane(pane_id)]);
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let (request_sender, _request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    let mut sent_border_moves = Vec::new();
    let mut pending_mouse_actions = Vec::new();
    let first_press_read_at = compute_instant_before_now(Duration::from_secs(10));

    for press_read_at in [first_press_read_at, Instant::now()] {
        assert_eq!(
            handle_terminal_input_event(
                &mut client,
                &mut uplink,
                &mut cell_size_query,
                Some(&mouse_frame),
                TerminalInput {
                    runtime_event: RuntimeEvent::MouseInput {
                        client_id,
                        mouse_input: build_mouse_press(get_content_cell(&mouse_frame, 0)),
                    },
                    read_at: press_read_at,
                },
                &mut sent_border_moves,
                &mut pending_mouse_actions,
            ),
            ControlFlow::Continue(())
        );
    }

    let clear_selection_action = MouseAction::Command(Box::new(Command::Visual(
        VisualCommand::ClearSelection(ClearSelectionArgs { pane_id }),
    )));
    assert_eq!(
        pending_mouse_actions,
        vec![clear_selection_action.clone(), clear_selection_action],
        "a double click would also highlight the word under the second press"
    );
}

/// Two frames of one `60x20` tab, drawn centered in the `80x22` pane area at
/// offset `(10, 1)`. The first holds `left_pane_id` at `(0, 0)` `29x20` and
/// `undocked_pane_id` at `(31, 0)` `29x20`. The second holds `left_pane_id`
/// alone at `(0, 0)` `60x20`, and `undocked_pane_id` floating at `(16, 4)`
/// `48x13`.
fn build_undocked_pane_frames(
    left_pane_id: PaneId,
    undocked_pane_id: PaneId,
) -> (PaintedFrame, PaintedFrame) {
    let build_frame_slot = |pane_id, outer_rect: Rect| FrameSlot {
        pane_id,
        outer_rect,
        content_rect: Some(outer_rect.compute_inner_with_border()),
        is_visible: true,
        is_suppressed: false,
    };
    let half_size = Size {
        column_count: 29,
        row_count: 20,
    };
    let mut initial_frame = build_test_painted_frame();
    initial_frame.session_snapshot.active_tab_snapshot.tab_size = Size {
        column_count: 60,
        row_count: 20,
    };
    initial_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots = vec![
        build_frame_slot(
            left_pane_id,
            Rect::from_origin_and_size(Point { column: 0, row: 0 }, half_size),
        ),
        build_frame_slot(
            undocked_pane_id,
            Rect::from_origin_and_size(Point { column: 31, row: 0 }, half_size),
        ),
    ];
    let mut undocked_frame = initial_frame.clone();
    undocked_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots = vec![build_frame_slot(
        left_pane_id,
        Rect::from_origin_and_size(
            Point { column: 0, row: 0 },
            Size {
                column_count: 60,
                row_count: 20,
            },
        ),
    )];
    let floating_pane_slot = FloatingPaneSlot::from_outer_rect(
        undocked_pane_id,
        Rect::from_origin_and_size(
            Point { column: 16, row: 4 },
            Size {
                column_count: 48,
                row_count: 13,
            },
        ),
        false,
    );
    undocked_frame.session_snapshot.floating_pane_slots = vec![FrameFloatingPaneSlot {
        pane_id: undocked_pane_id,
        outer_rect: floating_pane_slot.outer_rect,
        content_rect: floating_pane_slot.content_rect,
        is_pinned: false,
    }];
    undocked_frame.session_snapshot.floating_pane_ids = vec![undocked_pane_id];
    (initial_frame, undocked_frame)
}

/// Paint the first of [`build_undocked_pane_frames`] at `started_at`, record
/// the committed placement of its tab, and queue the second frame. Returns
/// the tab id and the queued second frame.
fn paint_undocked_pane_frames<B: Backend>(
    client: &mut Client,
    screen: &mut Screen<B>,
    undocked_pane_id: PaneId,
    started_at: Instant,
) -> (TabId, RenderSnapshot) {
    let (initial_frame, undocked_frame) =
        build_undocked_pane_frames(PaneId::new(), undocked_pane_id);
    screen.pending_snapshot = Some(build_render_snapshot(&build_received_painted_frame(
        initial_frame.clone(),
    )));
    screen
        .commit_pending_snapshot(client, started_at)
        .expect("the first frame paints");
    let active_tab_id = initial_frame.client_snapshot.active_tab_id;
    screen.note_committed_placement([Some(active_tab_id), None]);
    let undocked_snapshot = build_render_snapshot(&build_received_painted_frame(undocked_frame));
    screen.pending_snapshot = Some(undocked_snapshot.clone());
    (active_tab_id, undocked_snapshot)
}

#[test]
fn an_undocked_pane_slides_from_its_tiled_box_to_its_floating_box() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let undocked_pane_id = PaneId::new();
    let started_at = Instant::now();
    let (active_tab_id, undocked_snapshot) =
        paint_undocked_pane_frames(&mut client, &mut screen, undocked_pane_id, started_at);

    let undocked_mouse_frame = screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the undocked frame paints");
    assert_eq!(
        screen.shown_undock_floating_pane_slots,
        vec![FloatingPaneSlot::from_outer_rect(
            undocked_pane_id,
            Rect::from_origin_and_size(
                Point { column: 41, row: 1 },
                Size {
                    column_count: 29,
                    row_count: 20,
                },
            ),
            false,
        )],
        "the slide starts at the tiled box moved by the (10, 1) letterbox offset"
    );
    assert_eq!(
        undocked_mouse_frame.session_snapshot.floating_pane_slots,
        undocked_snapshot.session_snapshot.floating_pane_slots,
        "mouse input is placed against the floating box during the slide"
    );

    let halfway_time = started_at + Duration::from_millis(80);
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            halfway_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        Some(FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND)
    );
    screen
        .refresh_at(&mut client, Some(active_tab_id), halfway_time)
        .expect("the slide repaints without a session frame");
    assert_eq!(
        screen.shown_undock_floating_pane_slots,
        vec![FloatingPaneSlot::from_outer_rect(
            undocked_pane_id,
            Rect::from_origin_and_size(
                Point { column: 19, row: 4 },
                Size {
                    column_count: 46,
                    row_count: 14,
                },
            ),
            false,
        )],
        "80 ms into 160 ms the ease-out has covered 0.875 of the way"
    );

    let end_time = started_at + PLACEMENT_ANIMATION_DURATION;
    screen
        .refresh_at(&mut client, Some(active_tab_id), end_time)
        .expect("the slide's last repaint draws the floating box");
    assert_eq!(screen.shown_undock_floating_pane_slots, Vec::new());
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            end_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
    assert_eq!(
        screen.refresh_at(
            &mut client,
            Some(active_tab_id),
            end_time + FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None,
        "a finished slide draws nothing more"
    );
}

#[test]
fn reduced_motion_draws_an_undocked_pane_at_its_floating_box_at_once() {
    let mut client = build_test_client();
    client.client_config.should_reduce_motion = true;
    let mut screen = build_test_screen();
    let undocked_pane_id = PaneId::new();
    let started_at = Instant::now();
    paint_undocked_pane_frames(&mut client, &mut screen, undocked_pane_id, started_at);

    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the undocked frame paints");

    assert_eq!(screen.shown_undock_floating_pane_slots, Vec::new());
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            started_at,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
}

#[test]
fn a_frame_that_floats_one_pane_and_drops_another_slides_only_the_floated_pane() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let left_pane_id = PaneId::new();
    let undocked_pane_id = PaneId::new();
    let started_at = Instant::now();
    let (initial_frame, mut undocked_frame) =
        build_undocked_pane_frames(left_pane_id, undocked_pane_id);
    undocked_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .pane_id = PaneId::new();
    let slide_start_slot = FloatingPaneSlot::from_outer_rect(
        undocked_pane_id,
        Rect::from_origin_and_size(
            Point { column: 41, row: 1 },
            Size {
                column_count: 29,
                row_count: 20,
            },
        ),
        false,
    );
    let mut slide_start_frame = undocked_frame.clone();
    slide_start_frame.session_snapshot.floating_pane_slots = vec![FrameFloatingPaneSlot {
        pane_id: undocked_pane_id,
        outer_rect: slide_start_slot.outer_rect,
        content_rect: slide_start_slot.content_rect,
        is_pinned: false,
    }];
    screen.pending_snapshot = Some(build_render_snapshot(&build_received_painted_frame(
        initial_frame,
    )));
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the first frame paints");

    screen.pending_snapshot = Some(build_render_snapshot(&build_received_painted_frame(
        undocked_frame,
    )));
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the frame that floats one pane paints");

    assert_eq!(
        screen.shown_undock_floating_pane_slots,
        vec![slide_start_slot],
        "the left pane leaves the tab without a floating slot, so only the floated pane slides"
    );
    let mut slide_start_client = build_test_client();
    let mut slide_start_screen = build_test_screen();
    slide_start_screen.pending_snapshot = Some(build_render_snapshot(
        &build_received_painted_frame(slide_start_frame),
    ));
    slide_start_screen
        .commit_pending_snapshot(&mut slide_start_client, started_at)
        .expect("the frame with the floating pane at the slide's start paints");
    assert_eq!(
        screen.paint_driver.backend.buffer(),
        slide_start_screen.paint_driver.backend.buffer(),
        "the screen draws the floated pane at the slide's box"
    );
}

#[test]
fn a_frame_that_floats_two_panes_slides_both_from_their_tiled_boxes() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let left_pane_id = PaneId::new();
    let undocked_pane_id = PaneId::new();
    let started_at = Instant::now();
    let (initial_frame, mut undocked_frame) =
        build_undocked_pane_frames(left_pane_id, undocked_pane_id);
    undocked_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .pane_id = PaneId::new();
    let left_floating_pane_slot = FloatingPaneSlot::from_outer_rect(
        left_pane_id,
        Rect::from_origin_and_size(
            Point { column: 18, row: 5 },
            Size {
                column_count: 48,
                row_count: 13,
            },
        ),
        false,
    );
    undocked_frame.session_snapshot.floating_pane_slots.insert(
        0,
        FrameFloatingPaneSlot {
            pane_id: left_pane_id,
            outer_rect: left_floating_pane_slot.outer_rect,
            content_rect: left_floating_pane_slot.content_rect,
            is_pinned: false,
        },
    );
    undocked_frame
        .session_snapshot
        .floating_pane_ids
        .insert(0, left_pane_id);
    screen.pending_snapshot = Some(build_render_snapshot(&build_received_painted_frame(
        initial_frame,
    )));
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the first frame paints");

    screen.pending_snapshot = Some(build_render_snapshot(&build_received_painted_frame(
        undocked_frame,
    )));
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the frame that floats both panes paints");

    let tiled_half_size = Size {
        column_count: 29,
        row_count: 20,
    };
    assert_eq!(
        screen.shown_undock_floating_pane_slots,
        vec![
            FloatingPaneSlot::from_outer_rect(
                left_pane_id,
                Rect::from_origin_and_size(Point { column: 10, row: 1 }, tiled_half_size),
                false,
            ),
            FloatingPaneSlot::from_outer_rect(
                undocked_pane_id,
                Rect::from_origin_and_size(Point { column: 41, row: 1 }, tiled_half_size),
                false,
            ),
        ],
        "each pane starts its own slide at its tiled box moved by the (10, 1) letterbox offset"
    );
}

#[test]
fn a_paint_retry_keeps_one_slide_per_floated_pane() {
    let mut client = build_test_client();
    let mut screen = build_failing_screen();
    let undocked_pane_id = PaneId::new();
    let started_at = Instant::now();
    paint_undocked_pane_frames(&mut client, &mut screen, undocked_pane_id, started_at);
    screen.paint_driver.backend.should_fail_draw = true;
    assert_eq!(
        screen.commit_pending_snapshot(&mut client, started_at),
        None,
        "the backend rejects the frame, and the frame stays pending"
    );
    screen.paint_driver.backend.should_fail_draw = false;

    screen
        .commit_pending_snapshot(&mut client, started_at + Duration::from_millis(80))
        .expect("the pending frame paints on the next attempt");

    assert_eq!(
        screen.shown_undock_floating_pane_slots,
        vec![FloatingPaneSlot::from_outer_rect(
            undocked_pane_id,
            Rect::from_origin_and_size(
                Point { column: 19, row: 4 },
                Size {
                    column_count: 46,
                    row_count: 14,
                },
            ),
            false,
        )],
        "the slide that the rejected attempt started runs on from that attempt's time"
    );
}

/// The frames of [`build_undocked_pane_frames`] for a tab whose only pane is
/// `undocked_pane_id`: the first frame's tab holds the pane alone at
/// `(31, 0)` `29x20`. The second frame shows another tab, holding
/// `landing_pane_id` at `(0, 0)` `60x20`, with `undocked_pane_id` floating at
/// `(16, 4)` `48x13`.
fn build_sole_pane_undock_frames(
    landing_pane_id: PaneId,
    undocked_pane_id: PaneId,
) -> (PaintedFrame, PaintedFrame) {
    let (mut initial_frame, mut undocked_frame) =
        build_undocked_pane_frames(landing_pane_id, undocked_pane_id);
    initial_frame
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .retain(|pane_slot| pane_slot.pane_id == undocked_pane_id);
    let landing_tab_id = TabId::new();
    undocked_frame.session_snapshot.active_tab_snapshot.tab_id = landing_tab_id;
    undocked_frame.client_snapshot.active_tab_id = landing_tab_id;
    (initial_frame, undocked_frame)
}

#[test]
fn an_undock_slide_with_no_tab_slide_keeps_the_screen_waking() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let undocked_pane_id = PaneId::new();
    let started_at = Instant::now();
    let (initial_frame, undocked_frame) =
        build_sole_pane_undock_frames(PaneId::new(), undocked_pane_id);
    let source_tab_id = initial_frame.client_snapshot.active_tab_id;
    let landing_tab_id = undocked_frame.client_snapshot.active_tab_id;
    screen.pending_snapshot = Some(build_render_snapshot(&build_received_painted_frame(
        initial_frame,
    )));
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the first frame paints");
    screen.note_committed_placement([Some(source_tab_id), None]);
    screen.pending_snapshot = Some(build_render_snapshot(&build_received_painted_frame(
        undocked_frame,
    )));
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the undocked frame paints");
    assert!(
        screen.committed_placement_animation.is_none(),
        "the landing tab is not a tab the placement changed"
    );

    let halfway_time = started_at + Duration::from_millis(80);
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            halfway_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        Some(FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND)
    );
    screen
        .refresh_at(&mut client, Some(landing_tab_id), halfway_time)
        .expect("the slide repaints without a session frame");
    assert_eq!(
        screen.shown_undock_floating_pane_slots,
        vec![FloatingPaneSlot::from_outer_rect(
            undocked_pane_id,
            Rect::from_origin_and_size(
                Point { column: 19, row: 4 },
                Size {
                    column_count: 46,
                    row_count: 14,
                },
            ),
            false,
        )]
    );
}

#[test]
fn an_attached_viewer_redraws_an_undock_slide_with_no_new_session_frame() {
    let mut client = build_test_client();
    let client_id = client.get_client_id();
    let session_id = SessionId::new();
    let undocked_pane_id = PaneId::new();
    let (mut initial_frame, mut undocked_frame) =
        build_sole_pane_undock_frames(PaneId::new(), undocked_pane_id);
    let source_tab_id = initial_frame.client_snapshot.active_tab_id;
    for painted_frame in [&mut initial_frame, &mut undocked_frame] {
        painted_frame.session_snapshot.session_id = session_id;
        painted_frame.client_snapshot.client_id = client_id;
    }
    client.set_session_id(session_id);

    let draw_count = Arc::new(AtomicUsize::new(0));
    let failing_backend = FailingBackend::from_terminal_size_with_draw_count(
        TEST_VIEWPORT_SIZE,
        Arc::clone(&draw_count),
    );
    let mut screen = Screen::from_backend_and_viewport_size(failing_backend, TEST_VIEWPORT_SIZE);
    let (request_sender, _request_receiver) = mpsc::channel();
    let mut uplink = build_test_uplink(request_sender);
    let (incoming_sender, incoming_receiver) = build_incoming_channel();
    let producer_sender = incoming_sender.clone();
    let producer_draw_count = Arc::clone(&draw_count);
    let producer_thread = thread::spawn(move || -> Result<(), String> {
        let send_incoming = |incoming: Incoming| {
            producer_sender
                .send(incoming)
                .map_err(|send_error| send_error.to_string())
        };
        send_incoming(Incoming::Painted {
            connection_index: INITIAL_CONNECTION_INDEX,
            received_painted_frame: Box::new(build_received_painted_frame(initial_frame)),
        })?;
        if !wait_for_draw_count(&producer_draw_count, 1) {
            let _ = send_incoming(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
            return Err(String::from("the first frame was not drawn"));
        }
        send_incoming(build_session_event_incoming(
            SessionEvent::PanePlacementCommitted {
                command_id: CommandId::new(),
                source_pane_id: undocked_pane_id,
                source_tab_id: Some(source_tab_id),
                destination_tab_id: None,
                placement_target: PanePlacementTarget::Floating {
                    top_left_cell: None,
                },
            },
        ))?;
        send_incoming(Incoming::Painted {
            connection_index: INITIAL_CONNECTION_INDEX,
            received_painted_frame: Box::new(build_received_painted_frame(undocked_frame)),
        })?;
        if !wait_for_draw_count(&producer_draw_count, 2) {
            let _ = send_incoming(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
            return Err(String::from("the undocked frame was not drawn"));
        }
        if !wait_for_draw_count(&producer_draw_count, 3) {
            let _ = send_incoming(build_input_incoming(RuntimeEvent::Quit, Instant::now()));
            return Err(String::from(
                "no slide frame was drawn without a new session event",
            ));
        }
        send_incoming(build_session_event_incoming(SessionEvent::Detached))
    });
    let mut cell_size_query = terminal::CellSizeQuery::from_current_measurement(None, false, false);
    let attachment_ending = run_attachment(
        &build_local_home(),
        session_id,
        client_id,
        ConnectionToken::generate(),
        None,
        &mut client,
        &mut screen,
        &mut uplink,
        TEXT_ONLY_ATTACH_SETTINGS,
        &mut cell_size_query,
        &mut Vec::new(),
        incoming_sender,
        incoming_receiver,
    );
    producer_thread
        .join()
        .expect("the frame producer finished")
        .expect("the attachment drew both frames and a slide frame");

    assert_eq!(attachment_ending, AttachmentEnding::Detached);
}

#[test]
fn a_connection_reset_ends_running_undock_slides() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let undocked_pane_id = PaneId::new();
    let started_at = Instant::now();
    let (active_tab_id, _) =
        paint_undocked_pane_frames(&mut client, &mut screen, undocked_pane_id, started_at);
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the undocked frame paints");

    screen.reset_connection();
    let halfway_time = started_at + Duration::from_millis(80);
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            halfway_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
    screen
        .refresh_at(&mut client, Some(active_tab_id), halfway_time)
        .expect("the screen repaints the floating box");

    assert_eq!(screen.shown_undock_floating_pane_slots, Vec::new());
}

#[test]
fn a_placement_preview_ends_running_undock_slides() {
    let mut client = build_test_client();
    let mut screen = build_test_screen();
    let undocked_pane_id = PaneId::new();
    let started_at = Instant::now();
    let (active_tab_id, undocked_snapshot) =
        paint_undocked_pane_frames(&mut client, &mut screen, undocked_pane_id, started_at);
    screen
        .commit_pending_snapshot(&mut client, started_at)
        .expect("the undocked frame paints");
    let left_pane_id = undocked_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0]
        .pane_id;
    hold_same_tab_placement_preview(&mut client, &undocked_snapshot, left_pane_id);

    let halfway_time = started_at + Duration::from_millis(80);
    screen
        .refresh_at(&mut client, Some(active_tab_id), halfway_time)
        .expect("the placement preview paints");

    assert_eq!(
        screen.shown_undock_floating_pane_slots,
        Vec::new(),
        "the preview draws the floated pane at its floating box"
    );
    assert_eq!(
        screen.compute_next_placement_animation_wakeup_at(
            halfway_time,
            FRAME_INTERVAL_DURATION_AT_60_FRAMES_PER_SECOND
        ),
        None
    );
}
