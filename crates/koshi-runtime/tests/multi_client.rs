//! Integration tests for multiple clients attached to one session: what size
//! the tab they share gives its panes, how a client viewing another tab is
//! left out of that size, what the larger client is shown when the shared size
//! is smaller than its own terminal, whose lock mode a lock command changes,
//! which pane cell a mouse press names for the client that sent it, what a
//! client moving to another session leaves behind here, when the last client
//! moving away closes the session it left, and the pane each other client
//! types in as each frame lists it.
//!
//! They also cover how the keys of two clients typing into one pane reach it,
//! the name and number each client gets and the notices its arrival and
//! departure post, the keys and bindings the session drops once another
//! caller moved a client's input, what a session carried across a restart
//! gives back to each client, that one client changing its own view leaves
//! every other client's view as it was, and that a remote person's commands
//! get the answers a local client's commands get.

mod common;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use uuid::Uuid;

use common::{
    attach_test_client, build_fake_pty_backend_with_inbox, count_attached_clients,
    create_right_split_pane, get_last_painted_frame, get_seeded_tab_and_pane_ids,
    get_session_overview, open_remote_session_connection, open_session_connection,
    read_session_frames_to_detached, read_session_frames_to_mouse_answer,
    read_session_frames_until, read_session_frames_until_painted_frame,
    read_session_frames_until_painted_frame_reporting_no_paint, run_served_test_session,
    send_test_attach, serve_test_session, start_serving_test_session, start_test_session,
    submit_test_command, submit_test_command_for_answer, submit_test_command_for_client,
    wait_for_client_count, wait_for_mouse_tracking, AttachedTestClient, ViewerConnection,
    TEST_WAIT_TIMEOUT_DURATION,
};
use koshi_config::layer::PartialKoshiConfig;
use koshi_core::action::ActionReference;
use koshi_core::client::ClientOrigin;
use koshi_core::command::{
    ClearSelectionArgs, CloseFloatingPaneArgs, ClosePaneArgs, CloseTabArgs, Command,
    CommandEnvelope, CommandSource, DetachArgs, DockFloatingPaneArgs, FocusPaneArgs, FocusTabArgs,
    FocusTarget, LockModeArgs, MoveFloatingPaneArgs, MovePaneArgs, MoveTabArgs, NewPaneArgs,
    NewPanePlacement, NewTabArgs, PanePlacementTarget, PlacePaneArgs, QuitArgs, ResizePaneArgs,
    ScrollPaneArgs, SetAllFloatingPanesMinimizedArgs, SetFloatingPaneMinimizedArgs,
    SetFloatingPanePinnedArgs, SetSelectionArgs, SwitchSessionArgs, TabTarget,
    TiledPlacementTarget, ToggleFloatingPaneMinimizedArgs, ToggleFloatingPanePinnedArgs,
    ToggleLockModeArgs, VisualCommand, WriteToPaneArgs,
};
use koshi_core::event::{Event, InputModeChanged, PtyResized, RejectReason};
use koshi_core::geometry::{Direction, PaneArea, Point, Rect, Size};
use koshi_core::ids::{ClientId, CommandId, FloatingPaneId, PaneId, SessionId, TabId};
use koshi_core::key::{BindingModifierFlags, Key, KeyChord, NamedKey};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseKind};
use koshi_core::notification::{
    NotificationEntry, NotificationSeverity, NotificationSource, NotificationSubject,
};
use koshi_core::process::{PtySize, SpawnSpec};
use koshi_core::selection::{GridPosition, Selection, SelectionKind};
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::{
    FrameFloatingPane, FrameFloatingPaneSlot, FramePeer, FrameSlot, PaintedFrame,
};
use koshi_ipc::placement::PanePlacementPreview;
use koshi_ipc::protocol::{
    ConnectionToken, FiredAction, IpcRequest, IpcRequestKind, WireMouseAction,
};
use koshi_ipc::transport::Connection;
use koshi_layout::mode::LayoutMode;
use koshi_pty::backend::state::PtyBackend;
use koshi_runtime::resume::{read_resume_body, read_resume_header};
use koshi_runtime::runtime::painted_update::{
    ReceivedPaintedFrame, ReceivedPaneView, SessionFrame,
};
use koshi_runtime::runtime::peer_input::PEER_INPUT_MARK_DURATION;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::{
    build_floating_pane_id, build_key_input_for_chord, build_paint_report_request,
    send_attach_answer_received,
};

/// The PTY size [`common::TEST_VIEWPORT_SIZE`] gives the seeded session's single pane: one
/// tabline row and one hint row off the terminal, then a 1-cell pane border.
const SEEDED_PTY_SIZE: PtySize = PtySize {
    column_count: 78,
    row_count: 20,
};

/// The larger of the two viewports two clients share a tab at.
const LARGE_VIEWPORT_SIZE: Size = Size {
    column_count: 100,
    row_count: 40,
};

/// Smaller than [`LARGE_VIEWPORT_SIZE`] on both axes.
const SMALL_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 30,
};

/// The per-axis minimum of [`LARGE_VIEWPORT_SIZE`] and [`SMALL_VIEWPORT_SIZE`],
/// as a pane region: the size the tab holds while both clients view it.
const SHARED_PANE_VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 28,
};

/// Narrower than [`SHORT_VIEWPORT_SIZE`], and taller than it.
const NARROW_VIEWPORT_SIZE: Size = Size {
    column_count: 70,
    row_count: 40,
};

/// Wider than [`NARROW_VIEWPORT_SIZE`], and shorter than it.
const SHORT_VIEWPORT_SIZE: Size = Size {
    column_count: 100,
    row_count: 24,
};

/// The tab and the pane the [`Command::NewTab`] in `emitted_events` created. Panics
/// unless `emitted_events` holds an [`Event::PaneCreated`].
fn get_created_tab_and_pane_ids(emitted_events: &[Event]) -> (TabId, PaneId) {
    emitted_events
        .iter()
        .find_map(|emitted_event| match emitted_event {
            Event::PaneCreated(pane_created) => Some((
                pane_created.tab_id.expect("a new tab's pane is tiled"),
                pane_created.pane_id,
            )),
            _ => None,
        })
        .expect("the new tab reports its tab and its root pane")
}

#[test]
fn two_clients_on_one_tab_size_the_pty_to_the_per_axis_minimum() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "per-axis-minimum",
        |runtime_directory, session_id, _fake_pty_backend| {
            // The large client alone: the tab is its own pane region, 100 columns by
            // 38 rows, and the pane's PTY is that region minus its 1-cell border.
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut large_client_connection, 2, LARGE_VIEWPORT_SIZE);
            let (_, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);

            // The small client joins the same tab. It is narrower and shorter: it
            // takes both axes, and the pane's PTY shrinks on both.
            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut small_client_connection, 2, SMALL_VIEWPORT_SIZE);

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            assert_eq!(count_attached_clients(&mut caller_connection, 3), 2);

            (
                vec![
                    caller_connection,
                    large_client_connection,
                    small_client_connection,
                ],
                pane_id,
            )
        },
    );

    // Three resizes, in this order: the size the seeded session gave the pane,
    // the large client's own region when it attached, and the per-axis minimum of
    // the two clients once the small one joined.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 98,
                row_count: 36
            },
            PtySize {
                column_count: 78,
                row_count: 26
            },
        ],
    );
}

/// A client that reports a pane area smaller than its terminal sizes the tab
/// to that area.
#[test]
fn a_client_reporting_a_pane_area_sizes_the_pty_to_that_area() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "reported-pane-area",
        |runtime_directory, session_id, _fake_pty_backend| {
            let reported_pane_area = PaneArea::Reported(Size {
                column_count: 60,
                row_count: 20,
            });
            let mut client_connection = open_session_connection(&runtime_directory, session_id);
            let echoed_pane_area = send_test_attach(
                &mut client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                Some(reported_pane_area),
                None,
                None,
            )
            .pane_area;
            send_attach_answer_received(&mut client_connection, 3);
            let (_, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
            assert_eq!(echoed_pane_area, Some(reported_pane_area));

            (vec![client_connection], pane_id)
        },
    );

    // The seeded size, then the reported 60x20 region minus the pane's 1-cell
    // border on each side.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 58,
                row_count: 18
            }
        ],
    );
}

/// A client with no room to draw a pane contributes no size. The tab keeps the
/// size its other viewer gives it.
#[test]
fn a_starving_client_does_not_shrink_the_tab() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "starving-second",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            send_test_attach(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                None,
                None,
                None,
            );
            send_attach_answer_received(&mut large_client_connection, 3);
            let (_, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);

            let mut starving_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let echoed_pane_area = send_test_attach(
                &mut starving_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                Some(PaneArea::Starving),
                None,
                None,
            )
            .pane_area;
            send_attach_answer_received(&mut starving_client_connection, 3);
            assert_eq!(echoed_pane_area, Some(PaneArea::Starving));

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            wait_for_client_count(&mut caller_connection, 2, 3);

            (
                vec![
                    caller_connection,
                    large_client_connection,
                    starving_client_connection,
                ],
                pane_id,
            )
        },
    );

    // Two resizes only: the seeded size and the large client's own region. The
    // starving client moved nothing.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 98,
                row_count: 36
            }
        ],
    );
}

/// The starving client attaches first, and the tab has no viewer that sizes it
/// while the served loop renders. Nothing panics, that client's frames carry
/// every pane suppressed, and the seeded size stands until a sized client
/// arrives.
#[test]
fn a_starving_client_attaching_first_leaves_the_seeded_size_until_a_sized_client_arrives() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "starving-first",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut starving_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let echoed_pane_area = send_test_attach(
                &mut starving_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                Some(PaneArea::Starving),
                None,
                None,
            )
            .pane_area;
            send_attach_answer_received(&mut starving_client_connection, 3);
            let (_, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
            assert_eq!(echoed_pane_area, Some(PaneArea::Starving));

            // The served loop renders between the two attaches, with the tab's
            // only viewer contributing no pane area.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            wait_for_client_count(&mut caller_connection, 1, 3);

            // The tab has no viewer that sizes it: it solves at 0x0, and the
            // starving client's own frames carry its pane suppressed.
            let (starving_client_connection, session_frames) =
                read_session_frames_until(starving_client_connection, |session_frame| {
                    matches!(session_frame, SessionFrame::Painted(_))
                });
            let painted_frame = &get_last_painted_frame(&session_frames).painted_frame;
            assert_eq!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .layout_area
                    .solved_size,
                Size {
                    column_count: 0,
                    row_count: 0
                },
            );
            assert!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .layout_area
                    .is_every_pane_suppressed
            );
            assert_eq!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .layout_area
                    .pane_slots,
                vec![FrameSlot {
                    pane_id,
                    outer_rect: Rect {
                        origin: Point { column: 0, row: 0 },
                        size: Size {
                            column_count: 0,
                            row_count: 0
                        },
                    },
                    content_rect: None,
                    is_visible: false,
                    is_suppressed: true,
                }],
            );

            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            send_test_attach(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                None,
                None,
                None,
            );
            send_attach_answer_received(&mut large_client_connection, 3);
            wait_for_client_count(&mut caller_connection, 2, 1000);

            (
                vec![
                    caller_connection,
                    starving_client_connection.ipc_connection,
                    large_client_connection,
                ],
                pane_id,
            )
        },
    );

    // The seeded size stood while only the starving client viewed the tab,
    // then the large client's own region took over.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 98,
                row_count: 36
            }
        ],
    );
}

/// An attach that reports no pane area is echoed back as none.
#[test]
fn an_attach_without_a_pane_area_echoes_none() {
    let (_server, _fake_pty_backend, echoed_pane_area) = serve_test_session(
        "echoes-none",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut client_connection = open_session_connection(&runtime_directory, session_id);
            let echoed_pane_area = send_test_attach(
                &mut client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                None,
                None,
                None,
            )
            .pane_area;
            send_attach_answer_received(&mut client_connection, 3);
            (vec![client_connection], echoed_pane_area)
        },
    );

    assert_eq!(echoed_pane_area, None);
}

#[test]
fn each_axis_takes_its_minimum_from_a_different_client_and_grows_back_when_that_client_leaves() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "mixed-axis-minimum",
        |runtime_directory, session_id, _fake_pty_backend| {
            // The narrow client alone: 70 columns by 38 rows of pane region.
            let mut narrow_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut narrow_client_connection, 2, NARROW_VIEWPORT_SIZE);
            let (_, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);

            // The short client joins the same tab. It is wider but shorter: the
            // columns stay pinned by the narrow client and the rows drop to this
            // one's. Each axis takes its minimum from a different client.
            let mut short_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let short_client_id =
                attach_test_client(&mut short_client_connection, 2, SHORT_VIEWPORT_SIZE).client_id;

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            assert_eq!(count_attached_clients(&mut caller_connection, 3), 2);

            // The short client leaves. The narrow one is the only viewer left:
            // the rows grow back to its own region and the columns never move.
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::Detach(DetachArgs {
                    client_id: Some(short_client_id),
                }),
                4,
            );
            assert_eq!(
                emitted_events,
                vec![Event::PtyResized(PtyResized {
                    pane_id,
                    pty_size: PtySize {
                        column_count: 68,
                        row_count: 36
                    },
                })],
            );

            let (short_client_connection, session_frames) =
                read_session_frames_to_detached(short_client_connection);
            assert_eq!(
                session_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::Detached))
            );
            wait_for_client_count(&mut caller_connection, 1, 5);

            (
                vec![
                    caller_connection,
                    narrow_client_connection,
                    short_client_connection.ipc_connection,
                ],
                pane_id,
            )
        },
    );

    // Four resizes, in this order: the size the seeded session gave the pane,
    // the narrow client's own region, the mixed-axis minimum once the short
    // client joined, and the narrow client's region again once it left. The
    // columns are the narrow client's throughout.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 68,
                row_count: 36
            },
            PtySize {
                column_count: 68,
                row_count: 20
            },
            PtySize {
                column_count: 68,
                row_count: 36
            },
        ],
    );
}

#[test]
fn a_client_viewing_another_tab_never_constrains_this_tabs_size() {
    let (_server, fake_pty_backend, (first_pane_id, second_pane_id)) = serve_test_session(
        "per-tab-independence",
        |runtime_directory, session_id, _fake_pty_backend| {
            // The large client attaches to the seeded tab.
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let large_client_id =
                attach_test_client(&mut large_client_connection, 2, LARGE_VIEWPORT_SIZE).client_id;
            let (first_tab_id, first_pane_id) =
                get_seeded_tab_and_pane_ids(&runtime_directory, session_id);

            // A second tab, created for the large client, which moves onto it.
            // Its root pane spawns at the large client's own region.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::NewTab(NewTabArgs {
                    working_directory: None,
                    client_id: Some(large_client_id),
                }),
                3,
            );
            let (second_tab_id, second_pane_id) = get_created_tab_and_pane_ids(&emitted_events);

            // Send the large client back. It is the first tab's only viewer again,
            // and the second tab has none.
            submit_test_command(
                &mut caller_connection,
                session_id,
                Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Id(first_tab_id),
                    client_id: Some(large_client_id),
                }),
                4,
            );

            // The small client attaches. A fresh attach lands on the
            // lowest-indexed tab, which is the first one. The two clients share
            // it, and the small one takes both axes.
            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let small_client_id =
                attach_test_client(&mut small_client_connection, 2, SMALL_VIEWPORT_SIZE).client_id;
            assert_eq!(count_attached_clients(&mut caller_connection, 5), 2);

            // The small client switches to the second tab. The first tab is the
            // large client's alone again; the second tab is the small client's.
            submit_test_command(
                &mut caller_connection,
                session_id,
                Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Id(second_tab_id),
                    client_id: Some(small_client_id),
                }),
                6,
            );

            (
                vec![
                    caller_connection,
                    large_client_connection,
                    small_client_connection,
                ],
                (first_pane_id, second_pane_id),
            )
        },
    );

    // The first tab's pane: seeded, the large client's own region, down to the
    // shared minimum while the small client viewed it, and back to the large
    // client's region once the small one left for the other tab. The small
    // client viewing another tab adds nothing after that.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(first_pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 98,
                row_count: 36
            },
            PtySize {
                column_count: 78,
                row_count: 26
            },
            PtySize {
                column_count: 98,
                row_count: 36
            },
        ],
    );

    // The second tab's pane: spawned at the large client's region, then sized to
    // the small client's alone once that client switched onto it. The large
    // client viewing the first tab never bounds it.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(second_pane_id)
            .expect("the pane was spawned"),
        vec![
            PtySize {
                column_count: 98,
                row_count: 36
            },
            PtySize {
                column_count: 78,
                row_count: 26
            },
        ],
    );
}

#[test]
fn the_larger_client_sees_the_tab_letterboxed_at_the_shared_size() {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "letterbox",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let large_client_id =
                attach_test_client(&mut large_client_connection, 2, LARGE_VIEWPORT_SIZE).client_id;
            let (tab_id, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);

            // The small client joins the same tab and invalidates the layout. The
            // large client is sent a fresh frame at the size the two now share.
            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut small_client_connection, 2, SMALL_VIEWPORT_SIZE);

            let (large_client_connection, session_frames) = read_session_frames_until_tab_size(
                large_client_connection,
                SHARED_PANE_VIEWPORT_SIZE,
            );
            let painted_frame = &get_last_painted_frame(&session_frames).painted_frame;

            // The large client's own terminal is unchanged; the tab it draws is the
            // shared size, and the margin around it is the letterbox.
            assert_eq!(painted_frame.client_snapshot.client_id, large_client_id);
            assert_eq!(
                painted_frame.client_snapshot.viewport_size,
                LARGE_VIEWPORT_SIZE
            );
            assert_eq!(painted_frame.client_snapshot.active_tab_id, tab_id);
            assert_eq!(
                painted_frame.session_snapshot.active_tab_snapshot.tab_id,
                tab_id
            );
            assert_eq!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .layout_area
                    .solved_size,
                SHARED_PANE_VIEWPORT_SIZE
            );

            // The tab holds one pane, solved at origin (0, 0) over the shared size,
            // with its content inside a 1-cell border.
            assert_eq!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .layout_area
                    .pane_slots,
                vec![FrameSlot {
                    pane_id,
                    outer_rect: Rect {
                        origin: Point { column: 0, row: 0 },
                        size: SHARED_PANE_VIEWPORT_SIZE,
                    },
                    content_rect: Some(Rect {
                        origin: Point { column: 1, row: 1 },
                        size: Size {
                            column_count: 78,
                            row_count: 26
                        },
                    }),
                    is_visible: true,
                    is_suppressed: false,
                }],
            );
            assert!(
                !painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .layout_area
                    .is_every_pane_suppressed
            );

            (
                vec![
                    large_client_connection.ipc_connection,
                    small_client_connection,
                ],
                (),
            )
        },
    );
}

#[test]
fn locking_one_client_leaves_the_other_clients_lock_state_unchanged() {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "per-client-lock",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let large_client_id =
                attach_test_client(&mut large_client_connection, 2, LARGE_VIEWPORT_SIZE).client_id;

            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let small_client_id =
                attach_test_client(&mut small_client_connection, 2, SMALL_VIEWPORT_SIZE).client_id;

            // Lock the large client. Lock mode belongs to one client: the command
            // reports a single change that names that client alone.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::SetLockMode(LockModeArgs {
                    is_locked: true,
                    client_id: Some(large_client_id),
                }),
                3,
            );
            assert_eq!(
                emitted_events,
                vec![Event::InputModeChanged(InputModeChanged {
                    client_id: large_client_id,
                    lock_mode: LockMode::Locked,
                })],
            );

            // Lock the small client. Setting the mode a client already holds emits
            // nothing. This event shows the small client was still unlocked while
            // the large one was locked.
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::SetLockMode(LockModeArgs {
                    is_locked: true,
                    client_id: Some(small_client_id),
                }),
                4,
            );
            assert_eq!(
                emitted_events,
                vec![Event::InputModeChanged(InputModeChanged {
                    client_id: small_client_id,
                    lock_mode: LockMode::Locked,
                })],
            );

            (
                vec![
                    caller_connection,
                    large_client_connection,
                    small_client_connection,
                ],
                (),
            )
        },
    );
}

/// Setting the lock mode a client already holds applies and emits nothing;
/// setting the other mode emits the change.
#[test]
fn setting_the_lock_mode_a_client_already_holds_emits_nothing() {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "repeat-lock",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut client_connection = open_session_connection(&runtime_directory, session_id);
            let client_id =
                attach_test_client(&mut client_connection, 2, LARGE_VIEWPORT_SIZE).client_id;

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let build_lock_command = |is_locked| {
                Command::SetLockMode(LockModeArgs {
                    is_locked,
                    client_id: Some(client_id),
                })
            };

            assert_eq!(
                submit_test_command(
                    &mut caller_connection,
                    session_id,
                    build_lock_command(true),
                    3,
                ),
                vec![Event::InputModeChanged(InputModeChanged {
                    client_id,
                    lock_mode: LockMode::Locked,
                })],
            );
            assert_eq!(
                submit_test_command(
                    &mut caller_connection,
                    session_id,
                    build_lock_command(true),
                    4,
                ),
                Vec::<Event>::new(),
            );
            assert_eq!(
                submit_test_command(
                    &mut caller_connection,
                    session_id,
                    build_lock_command(false),
                    5,
                ),
                vec![Event::InputModeChanged(InputModeChanged {
                    client_id,
                    lock_mode: LockMode::Normal,
                })],
            );
            assert_eq!(
                submit_test_command(
                    &mut caller_connection,
                    session_id,
                    build_lock_command(false),
                    6,
                ),
                Vec::<Event>::new(),
            );

            (vec![caller_connection, client_connection], ())
        },
    );
}

/// [`read_session_frames_until_painted_frame`] stopping at a painted update
/// whose active tab is `tab_size`.
fn read_session_frames_until_tab_size(
    viewer_connection: impl Into<ViewerConnection>,
    tab_size: Size,
) -> (ViewerConnection, Vec<SessionFrame<SessionEvent>>) {
    read_session_frames_until_painted_frame(viewer_connection, move |received_painted_frame| {
        received_painted_frame
            .painted_frame
            .session_snapshot
            .active_tab_snapshot
            .layout_area
            .solved_size
            == tab_size
    })
}

/// Send one mouse round holding a single left press on `pane_id` at the
/// 0-based pane cell `pane_local_cell`, then read `viewer_connection`'s
/// stream until that round is answered.
fn send_mouse_press(
    viewer_connection: impl Into<ViewerConnection>,
    pane_id: PaneId,
    pane_local_cell: Point,
    request_id: u64,
) -> ViewerConnection {
    let mut viewer_connection = viewer_connection.into();
    viewer_connection
        .ipc_connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
                pane_id,
                mouse_kind: MouseKind::Press(MouseButton::Left),
                modifier_flags: BindingModifierFlags::NONE,
                pane_local_cell,
            }]),
        })
        .expect("send mouse round");
    let (viewer_connection, _) = read_session_frames_to_mouse_answer(viewer_connection, request_id);
    viewer_connection
}

#[test]
fn a_mouse_press_from_each_client_reaches_the_pane_cell_it_names() {
    // The tab both clients share is 80 by 28, so the pane's grid is 78x26.
    // The small client names the pane's 0-based cell (10, 5): the report reads
    // column 11, row 6. The large client names (0, 0): column 1, row 1.
    const SMALL_CLIENT_PANE_CELL: Point = Point { column: 10, row: 5 };
    const LARGE_CLIENT_PANE_CELL: Point = Point { column: 0, row: 0 };

    // A cell past the grid's last column and row. It is pulled to the pane's
    // column 78, row 26.
    const PAST_THE_GRID_PANE_CELL: Point = Point {
        column: 200,
        row: 200,
    };

    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "per-client-mouse",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut large_client_connection, 2, LARGE_VIEWPORT_SIZE);
            let (_, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);

            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut small_client_connection, 2, SMALL_VIEWPORT_SIZE);

            // The program in the pane asks for mouse reports. Until it has, a
            // forwarded event is written nowhere.
            let small_client_connection =
                wait_for_mouse_tracking(&fake_pty_backend, pane_id, small_client_connection);

            let small_client_connection =
                send_mouse_press(small_client_connection, pane_id, SMALL_CLIENT_PANE_CELL, 3);
            let large_client_connection =
                send_mouse_press(large_client_connection, pane_id, LARGE_CLIENT_PANE_CELL, 3);
            let large_client_connection =
                send_mouse_press(large_client_connection, pane_id, PAST_THE_GRID_PANE_CELL, 4);

            (
                vec![
                    large_client_connection.ipc_connection,
                    small_client_connection.ipc_connection,
                ],
                pane_id,
            )
        },
    );

    // Three reports, in the order the rounds ran.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        vec![
            b"\x1b[<0;11;6M".to_vec(),
            b"\x1b[<0;1;1M".to_vec(),
            b"\x1b[<0;78;26M".to_vec(),
        ],
    );
}

/// A press forwarded to a pane whose program has not asked for mouse reports
/// writes nothing to that pane's PTY. The round is still answered.
#[test]
fn a_mouse_press_before_the_pane_asks_for_reports_writes_nothing() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "mouse-before-tracking",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut client_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut client_connection, 2, LARGE_VIEWPORT_SIZE);
            let (_, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);

            // The client is the tab's only viewer, and it presses the pane's
            // 0-based cell (1, 2).
            let client_connection =
                send_mouse_press(client_connection, pane_id, Point { column: 1, row: 2 }, 3);

            (vec![client_connection.ipc_connection], pane_id)
        },
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane was spawned"),
        Vec::<Vec<u8>>::new(),
    );
}

#[test]
fn switching_session_detaches_the_client_here_and_lets_it_join_the_other_session() {
    // The pane region [`LARGE_VIEWPORT_SIZE`] gives the tab on its own, once the other client
    // is gone.
    const SINGLE_CLIENT_PANE_VIEWPORT_SIZE: Size = Size {
        column_count: 100,
        row_count: 38,
    };

    // One process serves one session. The session moved to is a second server,
    // seeded and served on its own thread. The moving side reads that session's
    // id off the first channel. The second channel tells the joining side that
    // the client has left the session it was in.
    let (target_session_id_sender, target_session_id_receiver) = mpsc::channel();
    let (source_client_left_sender, source_client_left_receiver) = mpsc::channel();

    let target_session_thread = std::thread::spawn(move || {
        let (
            target_server,
            target_fake_pty_backend,
            (target_session_id, target_client_id, target_pane_id),
        ) = serve_test_session(
            "switch-target",
            move |runtime_directory, session_id, _fake_pty_backend| {
                target_session_id_sender
                    .send(session_id)
                    .expect("the mover reads the id");
                source_client_left_receiver
                    .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
                    .expect("the client leaves the session it was in");

                // What the moved client does next, and all the router does for
                // it: open this session's socket and attach there.
                let mut joining_connection =
                    open_session_connection(&runtime_directory, session_id);
                let AttachedTestClient {
                    client_id: joined_client_id,
                    session_id: joined_session_id,
                    ..
                } = attach_test_client(&mut joining_connection, 2, SMALL_VIEWPORT_SIZE);
                assert_eq!(joined_session_id, session_id);
                let (_, joined_pane_id) =
                    get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
                (
                    vec![joining_connection],
                    (session_id, joined_client_id, joined_pane_id),
                )
            },
        );

        // The reply named a client this session minted for the attach.
        let session = target_server
            .list_sessions()
            .get(&target_session_id)
            .expect("session running");
        assert_eq!(session.clients.count_clients(), 1);
        assert_eq!(
            session
                .clients
                .get_client_by_id(target_client_id)
                .expect("minted client")
                .get_client_id(),
            target_client_id,
        );

        // Two resizes: the size the seeded session gave the pane, and the
        // joining client's own region once it attached.
        assert_eq!(
            target_fake_pty_backend
                .list_pane_sizes(target_pane_id)
                .expect("the pane was spawned"),
            vec![
                SEEDED_PTY_SIZE,
                PtySize {
                    column_count: 78,
                    row_count: 26
                }
            ],
        );
    });

    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "switch-source",
        move |runtime_directory, session_id, _fake_pty_backend| {
            let destination_session_id = target_session_id_receiver
                .recv_timeout(TEST_WAIT_TIMEOUT_DURATION)
                .expect("the other session is serving");

            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut large_client_connection, 2, LARGE_VIEWPORT_SIZE);
            let (_, source_pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);

            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let small_client_id =
                attach_test_client(&mut small_client_connection, 2, SMALL_VIEWPORT_SIZE).client_id;

            // Read the large client's stream past the shared size. The next frame
            // read after the move is one the move caused.
            let (large_client_connection, _) = read_session_frames_until_tab_size(
                large_client_connection,
                SHARED_PANE_VIEWPORT_SIZE,
            );

            // The move itself. It puts the other session on the moved client's own
            // queue, changes nothing here, and emits nothing.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::SwitchSession(SwitchSessionArgs {
                    client_id: Some(small_client_id),
                    session_id: destination_session_id,
                    owner_user_id: None,
                }),
                3,
            );
            assert_eq!(emitted_events, Vec::<Event>::new());

            // The moved client is told where to go on its event stream.
            let (small_client_connection, switch_session_frames) =
                read_session_frames_until(small_client_connection, |session_frame| {
                    matches!(
                        session_frame,
                        SessionFrame::Event(SessionEvent::SwitchTo { .. })
                    )
                });
            assert_eq!(
                switch_session_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::SwitchTo {
                    session_id: destination_session_id,
                    owner_user_id: None,
                })),
            );

            // The client leaves by closing its connection, the same as a real
            // client does once it has read where to go.
            drop(small_client_connection);
            source_client_left_sender
                .send(())
                .expect("the other session is waiting");

            // The tab grows back to the client that stayed, over one pane inside a
            // 1-cell border: this session keeps serving that client.
            let (large_client_connection, large_client_session_frames) =
                read_session_frames_until_tab_size(
                    large_client_connection,
                    SINGLE_CLIENT_PANE_VIEWPORT_SIZE,
                );
            assert_eq!(
                get_last_painted_frame(&large_client_session_frames)
                    .painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .layout_area
                    .pane_slots,
                vec![FrameSlot {
                    pane_id: source_pane_id,
                    outer_rect: Rect {
                        origin: Point { column: 0, row: 0 },
                        size: SINGLE_CLIENT_PANE_VIEWPORT_SIZE,
                    },
                    content_rect: Some(Rect {
                        origin: Point { column: 1, row: 1 },
                        size: Size {
                            column_count: 98,
                            row_count: 36
                        },
                    }),
                    is_visible: true,
                    is_suppressed: false,
                }],
            );

            (
                vec![caller_connection, large_client_connection.ipc_connection],
                source_pane_id,
            )
        },
    );

    // Four resizes, in this order: the size the seeded session gave the pane,
    // the staying client's own region, the shared minimum once the other client
    // joined the tab, and the staying client's region again once that client
    // moved away.
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            PtySize {
                column_count: 98,
                row_count: 36
            },
            PtySize {
                column_count: 78,
                row_count: 26
            },
            PtySize {
                column_count: 98,
                row_count: 36
            },
        ],
    );

    target_session_thread
        .join()
        .expect("the other session finished");
}

/// How long the loop in [`run_server_until_quit`] blocks on its inbox before it
/// reads the quit request again.
const QUIT_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// How long [`run_server_until_quit`] runs before it stops waiting for the quit
/// request. A session that never asks to close runs this long.
const QUIT_TIMEOUT_DURATION: Duration = Duration::from_secs(2);

/// Run `run_exchange` against a session from [`start_test_session`] with
/// `auto-close-session` set to `should_auto_close_session`. The loop reads the
/// quit request after every event and applies the inbox's quit hangup like any
/// other event. It stops when the quit request is set or
/// [`QUIT_TIMEOUT_DURATION`] has passed.
///
/// Returns the server, with the quit request it holds when the run ends.
fn run_server_until_quit(
    session_label: &str,
    should_auto_close_session: bool,
    run_exchange: impl FnOnce(PathBuf, SessionId) -> Vec<Connection> + Send + 'static,
) -> Server {
    let startup_config = PartialKoshiConfig {
        should_auto_close_session: Some(should_auto_close_session),
        ..PartialKoshiConfig::default()
    };
    let mut served_test_session = start_test_session(
        session_label,
        Some(startup_config),
        move |runtime_directory, session_id, _fake_pty_backend| {
            (run_exchange(runtime_directory, session_id), ())
        },
    );
    let server = &mut served_test_session.server;

    let deadline = Instant::now() + QUIT_TIMEOUT_DURATION;
    while !server.is_quit_requested() && Instant::now() < deadline {
        if let Ok(runtime_event) = server
            .get_inbox_receiver()
            .recv_timeout(QUIT_POLL_INTERVAL_DURATION)
        {
            let _ = server.handle_runtime_event(runtime_event);
        }
        server.push_due_frames(Instant::now());
    }

    let (server, _, ()) = served_test_session.finish();
    server
}

/// Attach one client at [`SMALL_VIEWPORT_SIZE`], move it to another session, and close its
/// connection: the whole of what a moved client does to the session it leaves.
/// Returns the caller connection, which is attached to nothing.
fn move_the_only_client_away(runtime_directory: PathBuf, session_id: SessionId) -> Vec<Connection> {
    // The session moved to is never read here: the id is put on the moved
    // client's queue, and that client reaches the other session itself.
    let destination_session_id = SessionId::new();

    let mut client_connection = open_session_connection(&runtime_directory, session_id);
    let client_id = attach_test_client(&mut client_connection, 2, SMALL_VIEWPORT_SIZE).client_id;

    let mut caller_connection = open_session_connection(&runtime_directory, session_id);
    let emitted_events = submit_test_command(
        &mut caller_connection,
        session_id,
        Command::SwitchSession(SwitchSessionArgs {
            client_id: Some(client_id),
            session_id: destination_session_id,
            owner_user_id: None,
        }),
        3,
    );
    assert_eq!(emitted_events, Vec::<Event>::new());

    let (client_connection, session_frames) =
        read_session_frames_until(client_connection, |session_frame| {
            matches!(
                session_frame,
                SessionFrame::Event(SessionEvent::SwitchTo { .. })
            )
        });
    assert_eq!(
        session_frames.last(),
        Some(&SessionFrame::Event(SessionEvent::SwitchTo {
            session_id: destination_session_id,
            owner_user_id: None,
        })),
    );

    drop(client_connection);
    vec![caller_connection]
}

#[test]
fn switching_the_last_client_away_closes_the_session_only_with_auto_close_on() {
    // The moved client was the only one attached. Its leaving empties the
    // session, and `auto-close-session` asks the process to quit.
    let auto_close_server =
        run_server_until_quit("switch-auto-close-on", true, move_the_only_client_away);
    assert!(
        auto_close_server.is_quit_requested(),
        "the emptied session was left running",
    );

    // The same move with the setting off: the session keeps running with no
    // client attached.
    let keep_open_server =
        run_server_until_quit("switch-auto-close-off", false, move_the_only_client_away);
    assert!(
        !keep_open_server.is_quit_requested(),
        "the emptied session asked to close",
    );
}

/// Alice and Bob view one session with terminals of one size. Alice's frames
/// list Bob typing in the seeded pane once he attaches. Bob reports that he
/// keeps his keys in a local input mode: Alice's next frame lists him typing
/// in no pane. Bob reports that he does not: Alice's next frame lists him in
/// the seeded pane again. Bob's connection ends: Alice's next frame lists no
/// peer.
#[test]
fn the_peer_list_in_a_viewers_frames_follows_another_viewer_attaching_changing_its_local_input_mode_and_leaving(
) {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "peer-list",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut alice_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut alice_connection, 2, SMALL_VIEWPORT_SIZE);
            let mut bob_connection = open_session_connection(&runtime_directory, session_id);
            let bob_client_id =
                attach_test_client(&mut bob_connection, 2, SMALL_VIEWPORT_SIZE).client_id;
            let (_tab_id, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
            let build_bob_peers = |typed_pane_id: Option<PaneId>| {
                vec![FramePeer {
                    client_id: bob_client_id,
                    color_index: 1,
                    label: "host-2".to_owned(),
                    origin: ClientOrigin::Local { is_same_user: true },
                    typed_pane_id,
                    recent_input_pane_ids: Vec::new(),
                }]
            };
            let read_until_peers =
                |viewer_connection: ViewerConnection, expected_peers: Vec<FramePeer>| {
                    let (viewer_connection, _) = read_session_frames_until_painted_frame(
                        viewer_connection,
                        move |received_painted_frame| {
                            received_painted_frame.painted_frame.session_snapshot.peers
                                == expected_peers
                        },
                    );
                    viewer_connection
                };

            let alice_connection =
                read_until_peers(alice_connection.into(), build_bob_peers(Some(pane_id)));
            bob_connection
                .send(&IpcRequest {
                    request_id: 3,
                    request_kind: IpcRequestKind::SetViewerLocalInputMode {
                        is_viewer_local_input_mode_on: true,
                    },
                })
                .expect("send the local input mode report");
            let alice_connection = read_until_peers(alice_connection, build_bob_peers(None));
            bob_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: IpcRequestKind::SetViewerLocalInputMode {
                        is_viewer_local_input_mode_on: false,
                    },
                })
                .expect("send the local input mode report");
            let alice_connection =
                read_until_peers(alice_connection, build_bob_peers(Some(pane_id)));
            drop(bob_connection);
            let alice_connection = read_until_peers(alice_connection, Vec::new());

            (vec![alice_connection.ipc_connection], ())
        },
    );
}

/// Bob turns his local input mode on, so frames name no pane for him. Alice
/// attaches, reads her first painted update and reports no paint, as a client
/// that paints slowly: every frame after it waits on her queue. Bob's `a` reaches
/// the seeded pane. More than two seconds after the key, Alice reports that update
/// painted and reads on: a frame names the seeded pane as Bob's recent input pane, and the
/// frame that names it no more arrives at least two seconds after she started
/// reading again.
#[test]
fn a_recent_input_pane_reaches_a_client_that_paints_late_and_stays_two_seconds_after_it_reads_on() {
    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "late-paint-recent-input",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut bob_connection = open_session_connection(&runtime_directory, session_id);
            let bob_client_id =
                attach_test_client(&mut bob_connection, 2, SMALL_VIEWPORT_SIZE).client_id;
            let (_tab_id, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
            send_viewer_local_input_mode(&mut bob_connection, 4, true);
            let (bob_connection, _) = read_until_earlier_requests_handled(bob_connection, 5);
            let build_bob_peers = move |recent_input_pane_ids: Vec<PaneId>| {
                vec![FramePeer {
                    client_id: bob_client_id,
                    color_index: 0,
                    label: "host".to_owned(),
                    origin: ClientOrigin::Local { is_same_user: true },
                    typed_pane_id: None,
                    recent_input_pane_ids,
                }]
            };
            let mut alice_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut alice_connection, 2, SMALL_VIEWPORT_SIZE);
            let expected_first_peers = build_bob_peers(Vec::new());
            let (mut alice_connection, alice_first_frames) =
                read_session_frames_until_painted_frame_reporting_no_paint(
                    alice_connection,
                    move |received_painted_frame| {
                        received_painted_frame.painted_frame.session_snapshot.peers
                            == expected_first_peers
                    },
                );
            let alice_first_update_sequence =
                get_last_painted_frame(&alice_first_frames).update_sequence;

            let mut bob_connection = bob_connection;
            bob_connection
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 6,
                    request_kind: build_tagged_character_request('a', 0),
                })
                .expect("send Bob's key");
            let (bob_connection, _) = read_until_earlier_requests_handled(bob_connection, 7);
            std::thread::sleep(PEER_INPUT_MARK_DURATION + Duration::from_secs(1));
            let reading_resumed_at = Instant::now();
            alice_connection
                .ipc_connection
                .send(&build_paint_report_request(alice_first_update_sequence))
                .expect("Alice reports her first update painted");
            let expected_marked_peers = build_bob_peers(vec![pane_id]);
            let (alice_connection, _) = read_session_frames_until_painted_frame(
                alice_connection,
                move |received_painted_frame| {
                    received_painted_frame.painted_frame.session_snapshot.peers
                        == expected_marked_peers
                },
            );
            let expected_cleared_peers = build_bob_peers(Vec::new());
            let (alice_connection, _) = read_session_frames_until_painted_frame(
                alice_connection,
                move |received_painted_frame| {
                    received_painted_frame.painted_frame.session_snapshot.peers
                        == expected_cleared_peers
                },
            );
            let mark_shown_duration = reading_resumed_at.elapsed();
            assert!(
                mark_shown_duration >= PEER_INPUT_MARK_DURATION,
                "{mark_shown_duration:?}"
            );

            (
                vec![
                    alice_connection.ipc_connection,
                    bob_connection.ipc_connection,
                ],
                (),
            )
        },
    );
}

/// How many keyboard requests each client sends in
/// [`two_clients_typing_into_one_pane_never_split_each_others_keys`].
const TYPED_REQUEST_COUNT_PER_CLIENT: usize = 200;

/// One keyboard request holding a press of `named_key`, then a press of each
/// digit of `request_number`, tagged with move count `0`. With cursor keys
/// in normal mode, `Up` and `17` write `ESC [ A 1 7` in one write.
fn build_numbered_keyboard_request(named_key: NamedKey, request_number: usize) -> IpcRequestKind {
    let mut key_inputs = vec![build_key_input_for_chord(KeyChord::from_parts(
        BindingModifierFlags::NONE,
        Key::Named(named_key),
    ))];
    key_inputs.extend(request_number.to_string().chars().map(|digit_character| {
        build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char(digit_character),
        ))
    }));
    IpcRequestKind::Keyboard {
        key_inputs,
        painted_input_pane_moved_by_others_count: 0,
    }
}

/// The bytes [`build_numbered_keyboard_request`] writes for
/// `cursor_key_bytes` and `request_number`, for request numbers `0` up to
/// [`TYPED_REQUEST_COUNT_PER_CLIENT`], in that order.
fn list_numbered_keyboard_request_bytes(cursor_key_bytes: &[u8]) -> Vec<Vec<u8>> {
    (0..TYPED_REQUEST_COUNT_PER_CLIENT)
        .map(|request_number| {
            let mut request_bytes = cursor_key_bytes.to_vec();
            request_bytes.extend(request_number.to_string().into_bytes());
            request_bytes
        })
        .collect()
}

/// Alice and Bob view one pane and type into it at the same time, each from a
/// thread of its own. Alice sends 200 requests of `Up` and a number, Bob 200
/// of `Down` and a number. The pane receives 400 writes: each write is one
/// whole request, and each client's writes come in the order it sent them.
#[test]
fn two_clients_typing_into_one_pane_never_split_each_others_keys() {
    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "interleaved-typing",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut alice_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut alice_connection, 2, SMALL_VIEWPORT_SIZE);
            let mut bob_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut bob_connection, 2, SMALL_VIEWPORT_SIZE);
            let (_, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);

            let start_typing = |mut typing_connection: Connection, named_key: NamedKey| {
                std::thread::spawn(move || {
                    for (request_id, request_number) in (3..).zip(0..TYPED_REQUEST_COUNT_PER_CLIENT)
                    {
                        typing_connection
                            .send(&IpcRequest {
                                request_id,
                                request_kind: build_numbered_keyboard_request(
                                    named_key,
                                    request_number,
                                ),
                            })
                            .expect("send keyboard request");
                    }
                    typing_connection
                })
            };
            let alice_typing = start_typing(alice_connection, NamedKey::Up);
            let bob_typing = start_typing(bob_connection, NamedKey::Down);
            let fence_request_id = 3 + u64::try_from(TYPED_REQUEST_COUNT_PER_CLIENT)
                .expect("a request count fits 64 bits");
            let (alice_viewer, _) = read_until_earlier_requests_handled(
                alice_typing.join().expect("Alice typed"),
                fence_request_id,
            );
            let (bob_viewer, _) = read_until_earlier_requests_handled(
                bob_typing.join().expect("Bob typed"),
                fence_request_id,
            );

            (
                vec![alice_viewer.ipc_connection, bob_viewer.ipc_connection],
                pane_id,
            )
        },
    );

    let pane_writes = fake_pty_backend
        .list_pane_write_bytes(pane_id)
        .expect("the pane was spawned");
    assert_eq!(pane_writes.len(), 2 * TYPED_REQUEST_COUNT_PER_CLIENT);
    let list_writes_starting_with = |cursor_key_bytes: &[u8]| -> Vec<Vec<u8>> {
        pane_writes
            .iter()
            .filter(|pane_write| pane_write.starts_with(cursor_key_bytes))
            .cloned()
            .collect()
    };
    assert_eq!(
        list_writes_starting_with(b"\x1b[A"),
        list_numbered_keyboard_request_bytes(b"\x1b[A"),
    );
    assert_eq!(
        list_writes_starting_with(b"\x1b[B"),
        list_numbered_keyboard_request_bytes(b"\x1b[B"),
    );
}

/// Send `Leaving` with `is_returning: false` on the attached
/// `ipc_connection` under `request_id`: the client leaves for good.
fn send_leaving_for_good(ipc_connection: &mut Connection, request_id: u64) {
    ipc_connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Leaving {
                is_returning: false,
            },
        })
        .expect("send leaving");
}

/// Read `viewer_connection`'s stream to its next posted notice, and return the
/// connection and the notice's entry.
fn read_next_posted_notice(
    viewer_connection: impl Into<ViewerConnection>,
) -> (ViewerConnection, NotificationEntry) {
    let (viewer_connection, session_frames) =
        read_session_frames_until(viewer_connection, |session_frame| {
            matches!(
                session_frame,
                SessionFrame::Event(SessionEvent::NotificationPosted { .. })
            )
        });
    let Some(SessionFrame::Event(SessionEvent::NotificationPosted { posted_entry, .. })) =
        session_frames.last()
    else {
        unreachable!("the read stops at a posted notice");
    };
    (viewer_connection, posted_entry.clone())
}

/// What a client reads of a join or leave notice: its source, its severity,
/// its text, the client it names and its repeat count.
fn describe_presence_notice(
    notice_entry: &NotificationEntry,
) -> (
    NotificationSource,
    NotificationSeverity,
    &str,
    Option<NotificationSubject>,
    u32,
) {
    (
        notice_entry.source,
        notice_entry.severity,
        notice_entry.text.as_str(),
        notice_entry.subject,
        notice_entry.repeat_count,
    )
}

/// Alice, of the host account, views the session alone, and Bob attaches from
/// another machine under the grant `bob`. Alice and Bob each read
/// `bob (2) joined remotely` once Bob confirms his attach answer. Bob's link
/// drops with no `Leaving`: Alice reads `bob (2) disconnected`. Bob dials
/// again with the token his attach minted, comes back as a new client under
/// the number 2, and Alice reads `bob (2) reconnected`: it takes the place of
/// the `disconnected` notice, under that notice's id at a count of 2. Bob
/// leaves for good:
/// Alice reads `bob (2) left`. Alice reads no other notice.
#[test]
fn a_remote_person_who_joins_drops_dials_again_and_leaves_is_announced_once_for_each() {
    serve_test_session(
        "remote-presence-notices",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut alice_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut alice_connection, 2, SMALL_VIEWPORT_SIZE);

            let mut bob_connection =
                open_remote_session_connection(&runtime_directory, session_id, "bob");
            let bob_attachment = attach_test_client(&mut bob_connection, 2, SMALL_VIEWPORT_SIZE);
            let (alice_connection, joined_notice) = read_next_posted_notice(alice_connection);
            assert_eq!(
                describe_presence_notice(&joined_notice),
                (
                    NotificationSource::Core,
                    NotificationSeverity::Info,
                    "bob (2) joined remotely",
                    Some(NotificationSubject::Client(bob_attachment.client_id)),
                    1,
                ),
            );
            let (bob_connection, bob_joined_notice) = read_next_posted_notice(bob_connection);
            assert_eq!(bob_joined_notice, joined_notice);

            drop(bob_connection);
            let (alice_connection, disconnected_notice) = read_next_posted_notice(alice_connection);
            assert_eq!(
                describe_presence_notice(&disconnected_notice),
                (
                    NotificationSource::Core,
                    NotificationSeverity::Info,
                    "bob (2) disconnected",
                    Some(NotificationSubject::Client(bob_attachment.client_id)),
                    1,
                ),
            );

            let mut redialed_bob_connection =
                open_remote_session_connection(&runtime_directory, session_id, "bob");
            let redialed_bob_attachment = send_test_attach(
                &mut redialed_bob_connection,
                2,
                SMALL_VIEWPORT_SIZE,
                None,
                None,
                bob_attachment.resume_token,
            );
            assert_ne!(redialed_bob_attachment.client_id, bob_attachment.client_id);
            send_attach_answer_received(&mut redialed_bob_connection, 3);
            let (alice_connection, reconnected_notice) = read_next_posted_notice(alice_connection);
            assert_eq!(
                describe_presence_notice(&reconnected_notice),
                (
                    NotificationSource::Core,
                    NotificationSeverity::Info,
                    "bob (2) reconnected",
                    Some(NotificationSubject::Client(
                        redialed_bob_attachment.client_id
                    )),
                    2,
                ),
            );
            assert_eq!(
                reconnected_notice.notification_id,
                disconnected_notice.notification_id
            );

            send_leaving_for_good(&mut redialed_bob_connection, 4);
            let (alice_connection, left_notice) = read_next_posted_notice(alice_connection);
            assert_eq!(
                describe_presence_notice(&left_notice),
                (
                    NotificationSource::Core,
                    NotificationSeverity::Info,
                    "bob (2) left",
                    Some(NotificationSubject::Client(
                        redialed_bob_attachment.client_id
                    )),
                    1,
                ),
            );
            let alice_connection =
                read_until_earlier_requests_handled_with_no_notice(alice_connection, 4);

            (
                vec![alice_connection.ipc_connection, redialed_bob_connection],
                (),
            )
        },
    );
}

/// Alice, of the host account, and Bob, from another machine, view the
/// session. Alice opens a second terminal on the session, then closes it for
/// good. Neither posts a notice: the next notice Alice reads is Bob's
/// `bob (2) left`.
#[test]
fn a_second_terminal_of_the_host_account_beside_a_remote_person_posts_no_notice() {
    serve_test_session(
        "second-host-notice",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut alice_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut alice_connection, 2, SMALL_VIEWPORT_SIZE);
            let mut bob_connection =
                open_remote_session_connection(&runtime_directory, session_id, "bob");
            let bob_client_id =
                attach_test_client(&mut bob_connection, 2, SMALL_VIEWPORT_SIZE).client_id;
            let (alice_connection, joined_notice) = read_next_posted_notice(alice_connection);
            assert_eq!(joined_notice.text, "bob (2) joined remotely");

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let mut second_alice_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut second_alice_connection, 2, SMALL_VIEWPORT_SIZE);
            send_leaving_for_good(&mut second_alice_connection, 4);
            // The session reads the receipt, then the leaving, on one
            // connection: both are read once the second terminal's record is
            // gone.
            wait_for_client_count(&mut caller_connection, 2, 2);

            send_leaving_for_good(&mut bob_connection, 4);
            let (alice_connection, left_notice) = read_next_posted_notice(alice_connection);
            assert_eq!(
                describe_presence_notice(&left_notice),
                (
                    NotificationSource::Core,
                    NotificationSeverity::Info,
                    "bob (2) left",
                    Some(NotificationSubject::Client(bob_client_id)),
                    1,
                ),
            );
            let alice_connection =
                read_until_earlier_requests_handled_with_no_notice(alice_connection, 4);

            (
                vec![
                    alice_connection.ipc_connection,
                    bob_connection,
                    second_alice_connection,
                    caller_connection,
                ],
                (),
            )
        },
    );
}

/// Two terminals of the host account and two people from other machines,
/// both granted as `bob`, attach in that order. The session lists them as
/// `host`, `host-2`, `bob` and `bob-2` under the indexes `0` to `3`,
/// and the notices name the two remote people `bob (3)` and `bob-2 (4)`.
#[test]
fn the_host_accounts_terminals_are_host_and_host_2_and_two_grants_named_bob_are_bob_and_bob_2() {
    serve_test_session(
        "client-names",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut host_connections = Vec::new();
            for _ in 0..2 {
                let mut host_connection = open_session_connection(&runtime_directory, session_id);
                attach_test_client(&mut host_connection, 2, SMALL_VIEWPORT_SIZE);
                host_connections.push(host_connection);
            }
            let mut first_bob_connection =
                open_remote_session_connection(&runtime_directory, session_id, "bob");
            attach_test_client(&mut first_bob_connection, 2, SMALL_VIEWPORT_SIZE);
            let first_host_connection = host_connections.remove(0);
            let (first_host_connection, first_bob_notice) =
                read_next_posted_notice(first_host_connection);
            assert_eq!(first_bob_notice.text, "bob (3) joined remotely");

            let mut second_bob_connection =
                open_remote_session_connection(&runtime_directory, session_id, "bob");
            attach_test_client(&mut second_bob_connection, 2, SMALL_VIEWPORT_SIZE);
            let (first_host_connection, second_bob_notice) =
                read_next_posted_notice(first_host_connection);
            assert_eq!(second_bob_notice.text, "bob-2 (4) joined remotely");
            let first_host_connection =
                read_until_earlier_requests_handled_with_no_notice(first_host_connection, 4);

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let mut listed_clients: Vec<(String, u32, ClientOrigin)> =
                get_session_overview(&mut caller_connection, 2)
                    .clients
                    .into_iter()
                    .map(|client_discovery| {
                        (
                            client_discovery.label,
                            client_discovery.color_index,
                            client_discovery.origin,
                        )
                    })
                    .collect();
            listed_clients.sort_by_key(|(_, color_index, _)| *color_index);
            assert_eq!(
                listed_clients,
                vec![
                    (
                        "host".to_string(),
                        0,
                        ClientOrigin::Local { is_same_user: true }
                    ),
                    (
                        "host-2".to_string(),
                        1,
                        ClientOrigin::Local { is_same_user: true }
                    ),
                    ("bob".to_string(), 2, ClientOrigin::Remote),
                    ("bob-2".to_string(), 3, ClientOrigin::Remote),
                ],
            );

            host_connections.push(first_host_connection.ipc_connection);
            host_connections.extend([
                first_bob_connection,
                second_bob_connection,
                caller_connection,
            ]);
            (host_connections, ())
        },
    );
}

/// The pane region [`LARGE_VIEWPORT_SIZE`] gives a tab it views alone: two
/// chrome rows off the terminal.
const LARGE_PANE_VIEWPORT_SIZE: Size = Size {
    column_count: 100,
    row_count: 38,
};

/// The PTY size [`LARGE_PANE_VIEWPORT_SIZE`] gives the seeded pane: a 1-cell
/// border off each side.
const LARGE_PANE_PTY_SIZE: PtySize = PtySize {
    column_count: 98,
    row_count: 36,
};

/// The PTY size [`SHARED_PANE_VIEWPORT_SIZE`] gives the seeded pane.
const SHARED_PANE_PTY_SIZE: PtySize = PtySize {
    column_count: 78,
    row_count: 26,
};

/// Read `viewer_connection`'s stream to its next painted update, and return
/// the connection and that update.
fn read_next_painted_frame(
    viewer_connection: impl Into<ViewerConnection>,
) -> (ViewerConnection, ReceivedPaintedFrame) {
    let (viewer_connection, session_frames) =
        read_session_frames_until_painted_frame(viewer_connection, |_| true);
    let next_painted_frame = get_last_painted_frame(&session_frames).clone();
    (viewer_connection, next_painted_frame)
}

/// How the smaller client's attachment ends in
/// [`each_way_the_smaller_client_ends_hands_the_tab_back_and_drops_its_peer_entry_in_one_frame`].
#[derive(Debug, Clone, Copy)]
enum SmallerClientEnding {
    /// The client sends `Leaving` with `is_returning: false`.
    LeavingForGood,
    /// The client sends `Leaving` with `is_returning: true`.
    LeavingToReturn,
    /// The client's connection closes with no `Leaving`.
    ConnectionDropped,
    /// A `Detach` command from another connection names the client.
    DetachCommand,
}

/// A large client views the seeded tab alone at 100x38 cells of pane region,
/// and a small client joins at 80x28. The large client's next frame lists the
/// small client typing in the seeded pane, at the shared 80x28. For each way
/// the small client's attachment ends, the large client's next frame lists no
/// peer at its own 100x38, and the pane's PTY returns to 98x36.
#[test]
fn each_way_the_smaller_client_ends_hands_the_tab_back_and_drops_its_peer_entry_in_one_frame() {
    for (ending_index, smaller_client_ending) in [
        SmallerClientEnding::LeavingForGood,
        SmallerClientEnding::LeavingToReturn,
        SmallerClientEnding::ConnectionDropped,
        SmallerClientEnding::DetachCommand,
    ]
    .into_iter()
    .enumerate()
    {
        let (_server, fake_pty_backend, pane_id) = serve_test_session(
            &format!("smaller-ends-{ending_index}"),
            move |runtime_directory, session_id, _fake_pty_backend| {
                let mut large_connection = open_session_connection(&runtime_directory, session_id);
                attach_test_client(&mut large_connection, 2, LARGE_VIEWPORT_SIZE);
                let (_tab_id, pane_id) =
                    get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
                let (large_connection, _) =
                    read_session_frames_until_tab_size(large_connection, LARGE_PANE_VIEWPORT_SIZE);

                let mut small_connection = open_session_connection(&runtime_directory, session_id);
                let small_client_id =
                    attach_test_client(&mut small_connection, 2, SMALL_VIEWPORT_SIZE).client_id;
                let (large_connection, joined_frame) = read_next_painted_frame(large_connection);
                assert_eq!(
                    (
                        joined_frame.painted_frame.session_snapshot.peers.clone(),
                        joined_frame
                            .painted_frame
                            .session_snapshot
                            .active_tab_snapshot
                            .layout_area
                            .solved_size,
                    ),
                    (
                        vec![FramePeer {
                            client_id: small_client_id,
                            color_index: 1,
                            label: "host-2".to_owned(),
                            origin: ClientOrigin::Local { is_same_user: true },
                            typed_pane_id: Some(pane_id),
                            recent_input_pane_ids: Vec::new(),
                        }],
                        SHARED_PANE_VIEWPORT_SIZE,
                    ),
                    "{smaller_client_ending:?}",
                );

                let mut caller_connection = open_session_connection(&runtime_directory, session_id);
                let mut open_connections = Vec::new();
                match smaller_client_ending {
                    SmallerClientEnding::LeavingForGood => {
                        send_leaving_for_good(&mut small_connection, 3);
                        open_connections.push(small_connection);
                    }
                    SmallerClientEnding::LeavingToReturn => {
                        small_connection
                            .send(&IpcRequest {
                                request_id: 3,
                                request_kind: IpcRequestKind::Leaving { is_returning: true },
                            })
                            .expect("send leaving");
                        open_connections.push(small_connection);
                    }
                    SmallerClientEnding::ConnectionDropped => drop(small_connection),
                    SmallerClientEnding::DetachCommand => {
                        submit_test_command(
                            &mut caller_connection,
                            session_id,
                            Command::Detach(DetachArgs {
                                client_id: Some(small_client_id),
                            }),
                            2,
                        );
                        open_connections.push(small_connection);
                    }
                }
                let (large_connection, left_frame) = read_next_painted_frame(large_connection);
                assert_eq!(
                    (
                        left_frame.painted_frame.session_snapshot.peers.clone(),
                        left_frame
                            .painted_frame
                            .session_snapshot
                            .active_tab_snapshot
                            .layout_area
                            .solved_size,
                    ),
                    (Vec::new(), LARGE_PANE_VIEWPORT_SIZE),
                    "{smaller_client_ending:?}",
                );

                open_connections.extend([large_connection.ipc_connection, caller_connection]);
                (open_connections, pane_id)
            },
        );

        assert_eq!(
            fake_pty_backend
                .list_pane_sizes(pane_id)
                .expect("the pane was spawned"),
            vec![
                SEEDED_PTY_SIZE,
                LARGE_PANE_PTY_SIZE,
                SHARED_PANE_PTY_SIZE,
                LARGE_PANE_PTY_SIZE,
            ],
            "{smaller_client_ending:?}",
        );
    }
}

/// A large client and a small client share the seeded tab at 80x28. The small
/// client's terminal grows to 90x35, which gives 90x33 of pane region, and the
/// tab follows to 90x33. It grows again to 120x50, past the large client, and
/// the tab stops at the large client's 100x38.
#[test]
fn the_shared_tab_follows_the_smaller_client_as_it_resizes_up_to_the_larger_clients_size() {
    const GROWN_VIEWPORT_SIZE: Size = Size {
        column_count: 90,
        row_count: 35,
    };
    const GROWN_PANE_VIEWPORT_SIZE: Size = Size {
        column_count: 90,
        row_count: 33,
    };
    const PAST_THE_LARGE_CLIENT_VIEWPORT_SIZE: Size = Size {
        column_count: 120,
        row_count: 50,
    };

    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "smaller-resizes",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut large_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut large_connection, 2, LARGE_VIEWPORT_SIZE);
            let (_, pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
            let mut small_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut small_connection, 2, SMALL_VIEWPORT_SIZE);
            let (large_connection, _) =
                read_session_frames_until_tab_size(large_connection, SHARED_PANE_VIEWPORT_SIZE);

            let send_resize =
                |small_connection: &mut Connection, request_id: u64, viewport_size| {
                    small_connection
                        .send(&IpcRequest {
                            request_id,
                            request_kind: IpcRequestKind::Resize {
                                viewport_size,
                                pane_area: None,
                                cell_size: None,
                            },
                        })
                        .expect("send resize");
                };
            send_resize(&mut small_connection, 3, GROWN_VIEWPORT_SIZE);
            let (large_connection, _) =
                read_session_frames_until_tab_size(large_connection, GROWN_PANE_VIEWPORT_SIZE);
            send_resize(
                &mut small_connection,
                4,
                PAST_THE_LARGE_CLIENT_VIEWPORT_SIZE,
            );
            let (large_connection, _) =
                read_session_frames_until_tab_size(large_connection, LARGE_PANE_VIEWPORT_SIZE);

            (
                vec![large_connection.ipc_connection, small_connection],
                pane_id,
            )
        },
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(pane_id)
            .expect("the pane was spawned"),
        vec![
            SEEDED_PTY_SIZE,
            LARGE_PANE_PTY_SIZE,
            SHARED_PANE_PTY_SIZE,
            PtySize {
                column_count: 88,
                row_count: 31,
            },
            LARGE_PANE_PTY_SIZE,
        ],
    );
}

/// Read `viewer_connection`'s stream until a painted update names
/// `typed_pane_id` as the pane its client's keys reach, and return the
/// connection and that update's move count.
fn read_until_typed_pane(
    viewer_connection: impl Into<ViewerConnection>,
    typed_pane_id: PaneId,
) -> (ViewerConnection, u64) {
    let (viewer_connection, session_frames) =
        read_session_frames_until_painted_frame(viewer_connection, move |received_painted_frame| {
            received_painted_frame
                .painted_frame
                .client_snapshot
                .typed_pane_id
                == Some(typed_pane_id)
        });
    let input_pane_moved_by_others_count = get_last_painted_frame(&session_frames)
        .painted_frame
        .client_snapshot
        .input_pane_moved_by_others_count;
    (viewer_connection, input_pane_moved_by_others_count)
}

/// One keyboard request holding a press of `key_character`, tagged with
/// `painted_input_pane_moved_by_others_count`.
fn build_tagged_character_request(
    key_character: char,
    painted_input_pane_moved_by_others_count: u64,
) -> IpcRequestKind {
    IpcRequestKind::Keyboard {
        key_inputs: vec![build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char(key_character),
        ))],
        painted_input_pane_moved_by_others_count,
    }
}

/// `koshi close-pane` of `pane_id`, killing its child at once.
fn build_force_close_pane_command(pane_id: PaneId) -> Command {
    Command::ClosePane(ClosePaneArgs {
        pane_id: Some(pane_id),
        should_force_close: true,
        should_kill_process_tree: false,
    })
}

/// Send an empty mouse round under `request_id` on `viewer_connection`, and
/// read its frames up to the round's answer. The session answers the round
/// after it handled every request that the connection sent before it.
fn read_until_earlier_requests_handled(
    viewer_connection: impl Into<ViewerConnection>,
    request_id: u64,
) -> (ViewerConnection, Vec<SessionFrame<SessionEvent>>) {
    let mut viewer_connection = viewer_connection.into();
    viewer_connection
        .ipc_connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Mouse(Vec::new()),
        })
        .expect("send the empty mouse round");
    read_session_frames_to_mouse_answer(viewer_connection, request_id)
}

/// [`read_until_earlier_requests_handled`], asserting that the frames read
/// hold no posted notice.
fn read_until_earlier_requests_handled_with_no_notice(
    viewer_connection: impl Into<ViewerConnection>,
    request_id: u64,
) -> ViewerConnection {
    let (viewer_connection, session_frames) =
        read_until_earlier_requests_handled(viewer_connection, request_id);
    let posted_notice_frames: Vec<_> = session_frames
        .iter()
        .filter(|session_frame| {
            matches!(
                session_frame,
                SessionFrame::Event(SessionEvent::NotificationPosted { .. })
            )
        })
        .collect();
    assert!(posted_notice_frames.is_empty(), "{posted_notice_frames:?}");
    viewer_connection
}

/// Alice and Bob view the seeded pane. A `koshi new-pane` for Bob splits it,
/// and Bob's keys reach the new right pane: his move count is 1. A
/// `koshi close-pane` closes the right pane, and Bob's keys reach the seeded
/// pane again: his move count is 2. Bob's `k`, typed against his screen with
/// count 1, is dropped, and his next frame counts 1 dropped request. Bob's `l`,
/// typed against the screen with count 2, reaches the seeded pane.
#[test]
fn a_key_typed_against_a_screen_from_before_another_caller_closed_its_pane_is_dropped_and_counted()
{
    let (_server, fake_pty_backend, (seeded_pane_id, right_pane_id)) = serve_test_session(
        "stale-key-dropped",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut alice_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut alice_connection, 2, SMALL_VIEWPORT_SIZE);
            let mut bob_connection = open_session_connection(&runtime_directory, session_id);
            let bob_client_id =
                attach_test_client(&mut bob_connection, 2, SMALL_VIEWPORT_SIZE).client_id;
            let (_, seeded_pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let right_pane_id = create_right_split_pane(
                &mut caller_connection,
                session_id,
                seeded_pane_id,
                bob_client_id,
                2,
            );
            let (bob_viewer, split_move_count) =
                read_until_typed_pane(bob_connection, right_pane_id);
            assert_eq!(split_move_count, 1);

            submit_test_command(
                &mut caller_connection,
                session_id,
                build_force_close_pane_command(right_pane_id),
                3,
            );
            let (mut bob_viewer, close_move_count) =
                read_until_typed_pane(bob_viewer, seeded_pane_id);
            assert_eq!(close_move_count, 2);

            bob_viewer
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 3,
                    request_kind: build_tagged_character_request('k', split_move_count),
                })
                .expect("send the stale key");
            let (mut bob_viewer, _) =
                read_session_frames_until_painted_frame(bob_viewer, |received_painted_frame| {
                    received_painted_frame
                        .painted_frame
                        .client_snapshot
                        .dropped_input_request_count
                        == 1
                });
            bob_viewer
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: build_tagged_character_request('l', close_move_count),
                })
                .expect("send the current key");
            let (bob_viewer, _) = read_until_earlier_requests_handled(bob_viewer, 5);

            (
                vec![
                    alice_connection,
                    bob_viewer.ipc_connection,
                    caller_connection,
                ],
                (seeded_pane_id, right_pane_id),
            )
        },
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(seeded_pane_id)
            .expect("the pane was spawned"),
        vec![b"l".to_vec()],
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(right_pane_id)
            .expect("the pane was spawned"),
        Vec::<Vec<u8>>::new(),
    );
}

/// Bob's keys reach the right pane of a split, with move count 1. Bob closes
/// that pane by key binding, and his keys reach the seeded pane with his move
/// count still 1: his own move counts nothing. His `l`, typed with count 1,
/// reaches the seeded pane.
#[test]
fn a_client_closing_its_own_pane_by_key_never_drops_its_next_key() {
    let (_server, fake_pty_backend, seeded_pane_id) = serve_test_session(
        "own-close-keeps-keys",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut alice_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut alice_connection, 2, SMALL_VIEWPORT_SIZE);
            let mut bob_connection = open_session_connection(&runtime_directory, session_id);
            let bob_client_id =
                attach_test_client(&mut bob_connection, 2, SMALL_VIEWPORT_SIZE).client_id;
            let (_, seeded_pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let right_pane_id = create_right_split_pane(
                &mut caller_connection,
                session_id,
                seeded_pane_id,
                bob_client_id,
                2,
            );
            let (mut bob_viewer, split_move_count) =
                read_until_typed_pane(bob_connection, right_pane_id);
            assert_eq!(split_move_count, 1);

            bob_viewer
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 3,
                    request_kind: IpcRequestKind::SubmitCommand {
                        command_envelope: Box::new(CommandEnvelope::from_parts(
                            CommandId::new(),
                            CommandSource::from_key_binding(bob_client_id),
                            build_force_close_pane_command(right_pane_id),
                        )),
                        fired_action: Some(FiredAction {
                            action_reference: ActionReference::from_core_action_name("close-pane")
                                .expect("the action name is valid"),
                            painted_input_pane_moved_by_others_count: split_move_count,
                        }),
                    },
                })
                .expect("send the close");
            let (mut bob_viewer, own_close_move_count) =
                read_until_typed_pane(bob_viewer, seeded_pane_id);
            assert_eq!(own_close_move_count, 1);

            bob_viewer
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: build_tagged_character_request('l', own_close_move_count),
                })
                .expect("send the key");
            let (bob_viewer, _) = read_until_earlier_requests_handled(bob_viewer, 5);

            (
                vec![
                    alice_connection,
                    bob_viewer.ipc_connection,
                    caller_connection,
                ],
                seeded_pane_id,
            )
        },
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(seeded_pane_id)
            .expect("the pane was spawned"),
        vec![b"l".to_vec()],
    );
}

/// Alice and Bob view the seeded pane, and a `koshi close-pane` closes the
/// right pane Bob types in, which moves Bob's keys with move count 2. Bob's
/// `core:new-tab` binding, fired against his screen with count 1, is refused
/// with `ViewChanged` to Bob, and no tab is created. Alice reads no
/// rejection: the frames she reads up to the next tab change hold none.
#[test]
fn a_binding_fired_against_an_old_screen_is_refused_to_its_typist_alone() {
    serve_test_session(
        "stale-binding-refused",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut alice_connection = open_session_connection(&runtime_directory, session_id);
            attach_test_client(&mut alice_connection, 2, SMALL_VIEWPORT_SIZE);
            let mut bob_connection = open_session_connection(&runtime_directory, session_id);
            let bob_client_id =
                attach_test_client(&mut bob_connection, 2, SMALL_VIEWPORT_SIZE).client_id;
            let (_, seeded_pane_id) = get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let right_pane_id = create_right_split_pane(
                &mut caller_connection,
                session_id,
                seeded_pane_id,
                bob_client_id,
                2,
            );
            let (bob_viewer, split_move_count) =
                read_until_typed_pane(bob_connection, right_pane_id);
            submit_test_command(
                &mut caller_connection,
                session_id,
                build_force_close_pane_command(right_pane_id),
                3,
            );
            let (mut bob_viewer, _) = read_until_typed_pane(bob_viewer, seeded_pane_id);

            let new_tab_command_id = CommandId::new();
            let new_tab_action = ActionReference::from_core_action_name("new-tab")
                .expect("the action name is valid");
            bob_viewer
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 3,
                    request_kind: IpcRequestKind::SubmitCommand {
                        command_envelope: Box::new(CommandEnvelope::from_parts(
                            new_tab_command_id,
                            CommandSource::from_key_binding(bob_client_id),
                            Command::NewTab(NewTabArgs {
                                working_directory: None,
                                client_id: None,
                            }),
                        )),
                        fired_action: Some(FiredAction {
                            action_reference: new_tab_action.clone(),
                            painted_input_pane_moved_by_others_count: split_move_count,
                        }),
                    },
                })
                .expect("send the stale binding");
            let (bob_viewer, bob_frames) = read_session_frames_until(bob_viewer, |session_frame| {
                matches!(
                    session_frame,
                    SessionFrame::Event(SessionEvent::CommandRejected { .. })
                )
            });
            assert_eq!(
                bob_frames.last(),
                Some(&SessionFrame::Event(SessionEvent::CommandRejected {
                    command_id: new_tab_command_id,
                    fired_action: Some(new_tab_action),
                    reject_reason: RejectReason::ViewChanged,
                    help: None,
                })),
            );

            let created_tab_id = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::NewTab(NewTabArgs {
                    working_directory: None,
                    client_id: Some(bob_client_id),
                }),
                4,
            );
            let (created_tab_id, _) = get_created_tab_and_pane_ids(&created_tab_id);
            let (alice_viewer, alice_frames) = read_session_frames_until_painted_frame(
                alice_connection,
                move |received_painted_frame| {
                    received_painted_frame
                        .painted_frame
                        .session_snapshot
                        .tab_snapshots
                        .iter()
                        .any(|tab_snapshot| tab_snapshot.tab_id == created_tab_id)
                },
            );
            assert_eq!(
                alice_frames
                    .iter()
                    .filter(|session_frame| matches!(
                        session_frame,
                        SessionFrame::Event(SessionEvent::CommandRejected { .. })
                    ))
                    .count(),
                0,
            );
            assert_eq!(
                alice_frames
                    .last()
                    .and_then(|session_frame| match session_frame {
                        SessionFrame::Painted(received_painted_frame) => Some(
                            received_painted_frame
                                .painted_frame
                                .session_snapshot
                                .tab_snapshots
                                .len()
                        ),
                        _ => None,
                    }),
                Some(2),
                "the refused binding created no tab",
            );

            (
                vec![
                    alice_viewer.ipc_connection,
                    bob_viewer.ipc_connection,
                    caller_connection,
                ],
                (),
            )
        },
    );
}

/// The fixture id `00000000-0000-7000-8000-abcd5000<suffix_number>` of
/// `format_five.json`, with `suffix_number` written as four decimal digits:
/// `5` is the first client, `10` the second client, `7`, `11` and `12`
/// panes.
fn build_format_five_uuid(suffix_number: u16) -> Uuid {
    Uuid::parse_str(&format!(
        "00000000-0000-7000-8000-abcd5000{suffix_number:04}"
    ))
    .expect("a format-5 fixture id")
}

/// The secret whose SHA-256 the format-5 fixture holds as the first client's
/// standing token.
const FORMAT_FIVE_FIRST_CLIENT_TOKEN_SECRET: &str = "format-five-first-client-token";

/// The secret the test gives the format-5 fixture's second client as its
/// standing token, and that secret's SHA-256.
const FORMAT_FIVE_SECOND_CLIENT_TOKEN_SECRET: &str = "format-five-second-client-claim-token";
const FORMAT_FIVE_SECOND_CLIENT_TOKEN_HASH: &str =
    "4b2047af9adb15e86b20b659ac0ea256089f6c19bddf54f2b142a9c03367c15c";

/// A session that a 0.6.0 build carried across its restart, read from the
/// format-5 resume fixture with the second client given a standing token,
/// restarts into this build with no client attached. Client 5 shows both
/// floating panes and pinned the 60% one at `(10, 3)`; client 10 minimized
/// it. Both viewers attach again over the socket, claiming their records with
/// their tokens. Each comes back under its id, its index and its label. Client
/// 5 shows the pinned floating pane holding its one pane, and client 10 shows it
/// minimized. Each frame lists the other client typing in the 40x10 floating
/// pane. A `koshi` call minimizes that floating pane for client 10, whose
/// keys move to pane 7 with move count 1, and client 10's `k` typed against
/// count 0 is dropped at once and writes nothing, and its `l` typed against
/// count 1 reaches pane 7. The fake terminal holds every carried pane. No claim
/// posts a notice.
#[test]
fn a_session_carried_by_0_6_0_comes_back_with_both_clients_their_numbers_views_presence_and_guard()
{
    let first_client_id = ClientId::from_uuid(build_format_five_uuid(5));
    let second_client_id = ClientId::from_uuid(build_format_five_uuid(10));
    let zoomed_pane_id = PaneId::from_uuid(build_format_five_uuid(7));
    let percent_pane_id = PaneId::from_uuid(build_format_five_uuid(11));
    let cells_pane_id = PaneId::from_uuid(build_format_five_uuid(12));

    let mut fixture_json: serde_json::Value =
        serde_json::from_slice(include_bytes!("../src/resume/fixtures/format_five.json"))
            .expect("the format-5 fixture is JSON");
    fixture_json["raw_body"]["saved_view_store"]["connection_token_hash_by_client_id"]
        [build_format_five_uuid(10).to_string()] =
        serde_json::json!(FORMAT_FIVE_SECOND_CLIENT_TOKEN_HASH);
    let resume_test_directory = tempfile::TempDir::new().expect("create the resume directory");
    let resume_file_path = resume_test_directory.path().join("session.resume");
    std::fs::write(
        &resume_file_path,
        serde_json::to_vec(&fixture_json).expect("the edited fixture serializes"),
    )
    .expect("write the resume file");
    let (resume_header, raw_body) =
        read_resume_header(&resume_file_path).expect("the header reads");
    let resume_body =
        read_resume_body(resume_header.resume_format, &raw_body).expect("the body reads");
    let (fake_pty_backend, runtime_event_sender, runtime_event_receiver) =
        build_fake_pty_backend_with_inbox();
    for carried_pane in &resume_header.carried_panes {
        fake_pty_backend
            .spawn_pane(
                carried_pane.pane_id,
                SpawnSpec::build_default_shell(None, BTreeMap::new()),
                carried_pane.get_pty_size(),
            )
            .expect("the fake backend opens the carried pane");
    }
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let resumed_server = Server::resume(
        pty_backend,
        runtime_event_receiver,
        Arc::default(),
        None,
        resume_body,
        resume_header.resume_format,
        resume_header
            .carried_panes
            .iter()
            .map(|carried_pane| (carried_pane.pane_id, carried_pane.get_pty_size()))
            .collect(),
        HashMap::new(),
    );

    run_served_test_session(start_serving_test_session(
        "carried-by-0-6-0",
        resumed_server,
        fake_pty_backend,
        runtime_event_sender,
        resume_header.session_id,
        move |runtime_directory, session_id, fake_pty_backend| {
            let mut first_connection = open_session_connection(&runtime_directory, session_id);
            let first_attachment = send_test_attach(
                &mut first_connection,
                2,
                Size {
                    column_count: 80,
                    row_count: 24,
                },
                None,
                Some(first_client_id),
                Some(ConnectionToken::from_secret(
                    FORMAT_FIVE_FIRST_CLIENT_TOKEN_SECRET,
                )),
            );
            send_attach_answer_received(&mut first_connection, 3);
            let mut second_connection = open_session_connection(&runtime_directory, session_id);
            let second_attachment = send_test_attach(
                &mut second_connection,
                2,
                Size {
                    column_count: 100,
                    row_count: 30,
                },
                None,
                Some(second_client_id),
                Some(ConnectionToken::from_secret(
                    FORMAT_FIVE_SECOND_CLIENT_TOKEN_SECRET,
                )),
            );
            send_attach_answer_received(&mut second_connection, 3);
            assert_eq!(
                (first_attachment.client_id, second_attachment.client_id),
                (first_client_id, second_client_id),
            );

            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let mut listed_clients: Vec<(ClientId, String, u32, ClientOrigin)> =
                get_session_overview(&mut caller_connection, 2)
                    .clients
                    .into_iter()
                    .map(|client_discovery| {
                        (
                            client_discovery.client_id,
                            client_discovery.label,
                            client_discovery.color_index,
                            client_discovery.origin,
                        )
                    })
                    .collect();
            listed_clients.sort_by_key(|(_, _, color_index, _)| *color_index);
            assert_eq!(
                listed_clients,
                vec![
                    (
                        first_client_id,
                        "C-\u{5d84}\u{65b0}-\u{767d}\u{9db4}".to_string(),
                        0,
                        ClientOrigin::Local { is_same_user: true },
                    ),
                    (
                        second_client_id,
                        "C-\u{3042}\u{3056}\u{3084}\u{304b}-\u{304b}\u{305c}".to_string(),
                        1,
                        ClientOrigin::Local { is_same_user: true },
                    ),
                ],
            );

            let (first_viewer, first_frames) = read_session_frames_until_painted_frame(
                first_connection,
                move |received_painted_frame| {
                    received_painted_frame.painted_frame.session_snapshot.peers
                        == vec![FramePeer {
                            client_id: second_client_id,
                            color_index: 1,
                            label: "C-\u{3042}\u{3056}\u{3084}\u{304b}-\u{304b}\u{305c}".to_owned(),
                            origin: ClientOrigin::Local { is_same_user: true },
                            typed_pane_id: Some(cells_pane_id),
                            recent_input_pane_ids: Vec::new(),
                        }]
                },
            );
            let first_frame = &get_last_painted_frame(&first_frames).painted_frame;
            assert_eq!(first_frame.client_snapshot.color_index, 0);
            let percent_floating_pane_id = build_floating_pane_id(percent_pane_id);
            let pinned_slot = find_floating_pane_slot(
                get_last_painted_frame(&first_frames),
                percent_floating_pane_id,
            )
            .expect("client 5 shows the 60% floating pane");
            assert_eq!(
                (
                    pinned_slot.is_pinned,
                    pinned_slot.outer_rect.origin,
                    pinned_slot
                        .layout_area
                        .pane_slots
                        .iter()
                        .map(|pane_slot| pane_slot.pane_id)
                        .collect::<Vec<PaneId>>(),
                ),
                (true, Point { column: 10, row: 3 }, vec![percent_pane_id]),
            );
            let percent_pane_title = first_frame
                .pane_snapshots
                .iter()
                .find(|pane_snapshot| pane_snapshot.pane_id == percent_pane_id)
                .expect("client 5 draws the 60% floating pane")
                .pane_title
                .clone();

            let (second_viewer, second_frames) = read_session_frames_until_painted_frame(
                second_connection,
                move |received_painted_frame| {
                    received_painted_frame.painted_frame.session_snapshot.peers
                        == vec![FramePeer {
                            client_id: first_client_id,
                            color_index: 0,
                            label: "C-\u{5d84}\u{65b0}-\u{767d}\u{9db4}".to_owned(),
                            origin: ClientOrigin::Local { is_same_user: true },
                            typed_pane_id: Some(cells_pane_id),
                            recent_input_pane_ids: Vec::new(),
                        }]
                },
            );
            let second_frame = &get_last_painted_frame(&second_frames).painted_frame;
            assert_eq!(second_frame.client_snapshot.color_index, 1);
            assert!(
                second_frame.session_snapshot.floating_panes.contains(
                    &FrameFloatingPane::Minimized {
                        floating_pane_id: percent_floating_pane_id,
                        pane_id: percent_pane_id,
                        pane_title: percent_pane_title,
                        pane_count: NonZeroUsize::MIN,
                        is_waiting: false,
                    }
                ),
                "client 10 shows the 60% floating pane minimized: {:?}",
                second_frame.session_snapshot.floating_panes,
            );

            submit_test_command_for_client(
                &mut caller_connection,
                session_id,
                Some(second_client_id),
                Command::SetFloatingPaneMinimized(SetFloatingPaneMinimizedArgs {
                    pane_id: Some(cells_pane_id),
                    is_minimized: true,
                }),
                3,
            );
            let (mut second_viewer, moved_count) =
                read_until_typed_pane(second_viewer, zoomed_pane_id);
            assert_eq!(moved_count, 1);
            second_viewer
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 4,
                    request_kind: build_tagged_character_request('k', 0),
                })
                .expect("send the stale key");
            let (mut second_viewer, second_frames_after_move) =
                read_session_frames_until_painted_frame(second_viewer, |received_painted_frame| {
                    received_painted_frame
                        .painted_frame
                        .client_snapshot
                        .dropped_input_request_count
                        == 1
                });
            second_viewer
                .ipc_connection
                .send(&IpcRequest {
                    request_id: 5,
                    request_kind: build_tagged_character_request('l', moved_count),
                })
                .expect("send the current key");
            let second_viewer =
                read_until_earlier_requests_handled_with_no_notice(second_viewer, 6);
            assert_eq!(
                fake_pty_backend
                    .list_pane_write_bytes(zoomed_pane_id)
                    .expect("the carried pane is open"),
                vec![b"l".to_vec()],
            );

            let posted_notice_frames: Vec<_> = first_frames
                .iter()
                .chain(&second_frames)
                .chain(&second_frames_after_move)
                .filter(|session_frame| {
                    matches!(
                        session_frame,
                        SessionFrame::Event(SessionEvent::NotificationPosted { .. })
                    )
                })
                .collect();
            assert!(posted_notice_frames.is_empty(), "{posted_notice_frames:?}");

            (
                vec![
                    first_viewer.ipc_connection,
                    second_viewer.ipc_connection,
                    caller_connection,
                ],
                (),
            )
        },
    ));
}

/// The panes and tabs every case of
/// [`a_client_changing_its_own_view_leaves_every_field_of_another_clients_view_as_it_was`]
/// starts from. The first tab holds a stack of the seeded pane and one more
/// pane, beside `right_pane_id`. One floating pane holds
/// `first_floating_pane_id` beside `second_floating_pane_id`. Alice views the
/// first tab and types in `second_floating_pane_id`. Bob views the first tab,
/// shows the floating pane tiled, unpinned and at its default place, and types
/// in `first_floating_pane_id`.
#[derive(Debug, Clone, Copy)]
struct IndependenceLayout {
    alice_client_id: ClientId,
    seeded_pane_id: PaneId,
    right_pane_id: PaneId,
    floating_pane_id: FloatingPaneId,
    first_floating_pane_id: PaneId,
    second_floating_pane_id: PaneId,
    second_tab_id: TabId,
}

/// The lines [`push_history_lines`] prints: more lines than a pane's rows hold.
const HISTORY_LINE_COUNT: usize = 60;

/// Make the program in `pane_id` print `line 0` through `line 59`, one line
/// each.
fn push_history_lines(fake_pty_backend: &FakePtyBackend, pane_id: PaneId) {
    let output_bytes: Vec<u8> = (0..HISTORY_LINE_COUNT)
        .flat_map(|line_number| format!("line {line_number}\r\n").into_bytes())
        .collect();
    fake_pty_backend
        .push_output(pane_id, output_bytes)
        .expect("the pane was spawned");
}

/// `koshi new-pane` for `client_id` at `new_pane_placement`, run over
/// `caller_connection`, and the new pane's id.
fn create_pane_for_client(
    caller_connection: &mut Connection,
    session_id: SessionId,
    client_id: ClientId,
    new_pane_placement: NewPanePlacement,
    request_id: u64,
) -> PaneId {
    submit_test_command(
        caller_connection,
        session_id,
        Command::NewPane(NewPaneArgs {
            placement: new_pane_placement,
            working_directory: None,
            spawn_spec: None,
            client_id: Some(client_id),
        }),
        request_id,
    )
    .iter()
    .find_map(|emitted_event| match emitted_event {
        Event::PaneCreated(pane_created) => Some(pane_created.pane_id),
        _ => None,
    })
    .expect("the new pane reports its id")
}

/// Send `SetViewerLocalInputMode` with `is_viewer_local_input_mode_on` on the
/// attached `ipc_connection` under `request_id`.
fn send_viewer_local_input_mode(
    ipc_connection: &mut Connection,
    request_id: u64,
    is_viewer_local_input_mode_on: bool,
) {
    ipc_connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::SetViewerLocalInputMode {
                is_viewer_local_input_mode_on,
            },
        })
        .expect("send the local input mode report");
}

/// Alice's entry in `received_painted_frame`'s peer list.
fn get_alice_peer(
    received_painted_frame: &ReceivedPaintedFrame,
    alice_client_id: ClientId,
) -> FramePeer {
    received_painted_frame
        .painted_frame
        .session_snapshot
        .peers
        .iter()
        .find(|peer| peer.client_id == alice_client_id)
        .expect("Bob shows Alice as a peer")
        .clone()
}

/// Everything a painted update shows its client, apart from its number and
/// the peer list.
fn describe_painted_view(
    received_painted_frame: &ReceivedPaintedFrame,
) -> (
    PaintedFrame,
    Vec<ReceivedPaneView>,
    Option<Arc<PanePlacementPreview>>,
) {
    let mut painted_frame = (*received_painted_frame.painted_frame).clone();
    painted_frame.session_snapshot.peers.clear();
    (
        painted_frame,
        received_painted_frame.pane_views.clone(),
        received_painted_frame.placement_preview.clone(),
    )
}

/// The slot `received_painted_frame` draws for `floating_pane_id`, if it
/// draws one.
fn find_floating_pane_slot(
    received_painted_frame: &ReceivedPaintedFrame,
    floating_pane_id: FloatingPaneId,
) -> Option<&FrameFloatingPaneSlot> {
    received_painted_frame
        .painted_frame
        .session_snapshot
        .floating_pane_slots
        .iter()
        .find(|floating_pane_slot| floating_pane_slot.floating_pane_id == floating_pane_id)
}

/// One case of
/// [`a_client_changing_its_own_view_leaves_every_field_of_another_clients_view_as_it_was`]:
/// its name, the commands run for Alice, and whether a frame of Alice's shows
/// what they changed.
struct IndependenceCase {
    case_name: &'static str,
    build_alice_commands: fn(&IndependenceLayout) -> Vec<Command>,
    is_shown_to_alice: fn(&IndependenceLayout, &ReceivedPaintedFrame) -> bool,
}

/// Each case changes one field that Alice's view owns, by commands that
/// Alice's own connection sends as key bindings, and Alice's next frames show
/// the change. Bob's frame, read
/// before the change and after it at the same point of Alice's local input
/// mode, is equal in every field: the layout, every pane's rows, cursor,
/// scroll position, highlight and title, the floating panes, the tab list,
/// his focus, lock mode and counts, and his placement preview.
#[test]
fn a_client_changing_its_own_view_leaves_every_field_of_another_clients_view_as_it_was() {
    let independence_cases = [
        IndependenceCase {
            case_name: "focus a tiled pane",
            build_alice_commands: |layout| {
                vec![Command::FocusPane(FocusPaneArgs {
                    focus_target: FocusTarget::Pane(layout.right_pane_id),
                    client_id: Some(layout.alice_client_id),
                })]
            },
            is_shown_to_alice: |layout, frame| {
                frame.painted_frame.client_snapshot.focused_pane_id == Some(layout.right_pane_id)
            },
        },
        IndependenceCase {
            case_name: "zoom a tiled pane",
            build_alice_commands: |layout| {
                vec![
                    Command::FocusPane(FocusPaneArgs {
                        focus_target: FocusTarget::Pane(layout.right_pane_id),
                        client_id: Some(layout.alice_client_id),
                    }),
                    Command::TogglePaneFullscreen,
                ]
            },
            is_shown_to_alice: |layout, frame| {
                frame
                    .painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .layout_area
                    .layout_mode
                    == LayoutMode::Fullscreen {
                        focused_pane_id: layout.right_pane_id,
                    }
            },
        },
        IndependenceCase {
            case_name: "open another stack member",
            build_alice_commands: |layout| {
                vec![Command::FocusPane(FocusPaneArgs {
                    focus_target: FocusTarget::Pane(layout.seeded_pane_id),
                    client_id: Some(layout.alice_client_id),
                })]
            },
            is_shown_to_alice: |layout, frame| {
                frame.painted_frame.client_snapshot.focused_pane_id == Some(layout.seeded_pane_id)
            },
        },
        IndependenceCase {
            case_name: "focus another pane inside the floating pane",
            build_alice_commands: |layout| {
                vec![Command::FocusPane(FocusPaneArgs {
                    focus_target: FocusTarget::Pane(layout.first_floating_pane_id),
                    client_id: Some(layout.alice_client_id),
                })]
            },
            is_shown_to_alice: |layout, frame| {
                frame.painted_frame.client_snapshot.focused_pane_id
                    == Some(layout.first_floating_pane_id)
            },
        },
        IndependenceCase {
            case_name: "zoom a pane inside the floating pane",
            build_alice_commands: |_| vec![Command::TogglePaneFullscreen],
            is_shown_to_alice: |layout, frame| {
                find_floating_pane_slot(frame, layout.floating_pane_id).is_some_and(
                    |floating_pane_slot| {
                        floating_pane_slot.layout_area.layout_mode
                            == LayoutMode::Fullscreen {
                                focused_pane_id: layout.second_floating_pane_id,
                            }
                    },
                )
            },
        },
        IndependenceCase {
            case_name: "scroll a pane's history",
            build_alice_commands: |layout| {
                vec![Command::ScrollPane(ScrollPaneArgs {
                    pane_id: Some(layout.right_pane_id),
                    scroll_line_count: 5,
                })]
            },
            is_shown_to_alice: |layout, frame| {
                frame
                    .pane_views
                    .iter()
                    .find(|pane_view| pane_view.pane_id == layout.right_pane_id)
                    .is_some_and(|pane_view| {
                        pane_view.view_top_row_index + 5
                            == u64::try_from(pane_view.retained_line_count)
                                .expect("a line count fits 64 bits")
                    })
            },
        },
        IndependenceCase {
            case_name: "highlight text",
            build_alice_commands: |layout| {
                vec![Command::Visual(VisualCommand::SetSelection(
                    SetSelectionArgs {
                        pane_id: layout.right_pane_id,
                        selection: Selection {
                            selection_kind: SelectionKind::Character,
                            anchor: GridPosition {
                                row_index: 0,
                                column_index: 0,
                            },
                            cursor: GridPosition {
                                row_index: 0,
                                column_index: 3,
                            },
                        },
                    },
                ))]
            },
            is_shown_to_alice: |layout, frame| {
                frame
                    .painted_frame
                    .pane_snapshots
                    .iter()
                    .any(|pane_snapshot| {
                        pane_snapshot.pane_id == layout.right_pane_id && pane_snapshot.has_selection
                    })
            },
        },
        IndependenceCase {
            case_name: "grab the mouse for selection",
            build_alice_commands: |_| vec![Command::ToggleMouseSelect],
            is_shown_to_alice: |_, frame| {
                frame
                    .painted_frame
                    .client_snapshot
                    .is_mouse_selection_enabled
            },
        },
        IndependenceCase {
            case_name: "view another tab",
            build_alice_commands: |layout| {
                vec![Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Id(layout.second_tab_id),
                    client_id: Some(layout.alice_client_id),
                })]
            },
            is_shown_to_alice: |layout, frame| {
                frame.painted_frame.client_snapshot.active_tab_id == layout.second_tab_id
            },
        },
        IndependenceCase {
            case_name: "move the floating pane",
            build_alice_commands: |layout| {
                vec![Command::MoveFloatingPane(MoveFloatingPaneArgs {
                    pane_id: layout.first_floating_pane_id,
                    top_left_cell: Point { column: 5, row: 2 },
                })]
            },
            is_shown_to_alice: |layout, frame| {
                find_floating_pane_slot(frame, layout.floating_pane_id).is_some_and(
                    |floating_pane_slot| {
                        floating_pane_slot.outer_rect.origin == Point { column: 5, row: 2 }
                    },
                )
            },
        },
        IndependenceCase {
            case_name: "pin the floating pane",
            build_alice_commands: |layout| {
                vec![Command::SetFloatingPanePinned(SetFloatingPanePinnedArgs {
                    pane_id: Some(layout.first_floating_pane_id),
                    is_pinned: true,
                })]
            },
            is_shown_to_alice: |layout, frame| {
                find_floating_pane_slot(frame, layout.floating_pane_id)
                    .is_some_and(|floating_pane_slot| floating_pane_slot.is_pinned)
            },
        },
        IndependenceCase {
            case_name: "minimize the floating pane",
            build_alice_commands: |layout| {
                vec![Command::SetFloatingPaneMinimized(
                    SetFloatingPaneMinimizedArgs {
                        pane_id: Some(layout.first_floating_pane_id),
                        is_minimized: true,
                    },
                )]
            },
            is_shown_to_alice: |layout, frame| {
                frame
                    .painted_frame
                    .session_snapshot
                    .floating_panes
                    .iter()
                    .any(|floating_pane| match floating_pane {
                        FrameFloatingPane::Minimized {
                            floating_pane_id, ..
                        } => *floating_pane_id == layout.floating_pane_id,
                        FrameFloatingPane::Shown { .. } => false,
                    })
            },
        },
    ];

    for (case_index, independence_case) in independence_cases.into_iter().enumerate() {
        serve_test_session(
            &format!("independence-{case_index}"),
            move |runtime_directory, session_id, fake_pty_backend| {
                let case_name = independence_case.case_name;
                let mut alice_connection = open_session_connection(&runtime_directory, session_id);
                let alice_client_id =
                    attach_test_client(&mut alice_connection, 2, LARGE_VIEWPORT_SIZE).client_id;
                let mut bob_connection = open_session_connection(&runtime_directory, session_id);
                let bob_client_id =
                    attach_test_client(&mut bob_connection, 2, LARGE_VIEWPORT_SIZE).client_id;
                let (seeded_tab_id, seeded_pane_id) =
                    get_seeded_tab_and_pane_ids(&runtime_directory, session_id);

                let mut caller_connection = open_session_connection(&runtime_directory, session_id);
                let right_pane_id = create_right_split_pane(
                    &mut caller_connection,
                    session_id,
                    seeded_pane_id,
                    alice_client_id,
                    2,
                );
                create_pane_for_client(
                    &mut caller_connection,
                    session_id,
                    alice_client_id,
                    NewPanePlacement::Stacked {
                        source_pane_id: Some(seeded_pane_id),
                        tab_id: None,
                    },
                    3,
                );
                let first_floating_pane_id = create_pane_for_client(
                    &mut caller_connection,
                    session_id,
                    alice_client_id,
                    NewPanePlacement::Floating {
                        size: None,
                        top_left_cell: None,
                        is_pinned: false,
                    },
                    4,
                );
                let second_floating_pane_id = create_pane_for_client(
                    &mut caller_connection,
                    session_id,
                    alice_client_id,
                    NewPanePlacement::Split {
                        source_pane_id: Some(first_floating_pane_id),
                        tab_id: None,
                        direction: Direction::Right,
                    },
                    5,
                );
                let (second_tab_id, _) = get_created_tab_and_pane_ids(&submit_test_command(
                    &mut caller_connection,
                    session_id,
                    Command::NewTab(NewTabArgs {
                        working_directory: None,
                        client_id: Some(alice_client_id),
                    }),
                    6,
                ));
                submit_test_command(
                    &mut caller_connection,
                    session_id,
                    Command::FocusTab(FocusTabArgs {
                        focus_target: TabTarget::Id(seeded_tab_id),
                        client_id: Some(alice_client_id),
                    }),
                    7,
                );
                submit_test_command(
                    &mut caller_connection,
                    session_id,
                    Command::FocusPane(FocusPaneArgs {
                        focus_target: FocusTarget::Pane(second_floating_pane_id),
                        client_id: Some(alice_client_id),
                    }),
                    8,
                );
                submit_test_command_for_client(
                    &mut caller_connection,
                    session_id,
                    Some(bob_client_id),
                    Command::SetFloatingPaneMinimized(SetFloatingPaneMinimizedArgs {
                        pane_id: Some(first_floating_pane_id),
                        is_minimized: false,
                    }),
                    9,
                );
                push_history_lines(&fake_pty_backend, right_pane_id);
                let (mut alice_viewer, alice_setup_frames) =
                    read_session_frames_until_painted_frame(
                        alice_connection,
                        move |received_painted_frame| {
                            let painted_frame = &received_painted_frame.painted_frame;
                            painted_frame.session_snapshot.tab_snapshots.len() == 2
                                && painted_frame.client_snapshot.active_tab_id == seeded_tab_id
                                && painted_frame.client_snapshot.focused_pane_id
                                    == Some(second_floating_pane_id)
                        },
                    );
                let [FrameFloatingPane::Shown {
                    floating_pane_id, ..
                }] = get_last_painted_frame(&alice_setup_frames)
                    .painted_frame
                    .session_snapshot
                    .floating_panes[..]
                else {
                    panic!("Alice shows the one floating pane");
                };
                let layout = IndependenceLayout {
                    alice_client_id,
                    seeded_pane_id,
                    right_pane_id,
                    floating_pane_id,
                    first_floating_pane_id,
                    second_floating_pane_id,
                    second_tab_id,
                };

                let is_alice_typing_in_no_pane =
                    move |received_painted_frame: &ReceivedPaintedFrame| {
                        let alice_peer = get_alice_peer(received_painted_frame, alice_client_id);
                        alice_peer.color_index == 0
                            && alice_peer.label == "host"
                            && alice_peer.origin == ClientOrigin::Local { is_same_user: true }
                            && alice_peer.typed_pane_id.is_none()
                            && alice_peer.recent_input_pane_ids.is_empty()
                    };
                let (bob_viewer, _) = read_session_frames_until_painted_frame(
                    bob_connection,
                    move |received_painted_frame| {
                        received_painted_frame.pane_views.iter().any(|pane_view| {
                            pane_view.pane_id == right_pane_id && pane_view.retained_line_count > 0
                        })
                    },
                );
                send_viewer_local_input_mode(&mut alice_viewer.ipc_connection, 3, true);
                let (bob_viewer, bob_frames_before) =
                    read_session_frames_until_painted_frame(bob_viewer, is_alice_typing_in_no_pane);
                let bob_frame_before = get_last_painted_frame(&bob_frames_before).clone();
                let bob_floating_pane_slot =
                    find_floating_pane_slot(&bob_frame_before, floating_pane_id)
                        .expect("Bob shows the floating pane");
                assert_eq!(
                    (
                        bob_floating_pane_slot.pane_id,
                        bob_floating_pane_slot.is_pinned,
                        bob_floating_pane_slot.layout_area.layout_mode,
                    ),
                    (first_floating_pane_id, false, LayoutMode::Tiled),
                );
                send_viewer_local_input_mode(&mut alice_viewer.ipc_connection, 4, false);
                let (bob_viewer, _) = read_session_frames_until_painted_frame(
                    bob_viewer,
                    move |received_painted_frame| {
                        !is_alice_typing_in_no_pane(received_painted_frame)
                    },
                );

                for (request_id, alice_command) in
                    (6..).zip((independence_case.build_alice_commands)(&layout))
                {
                    alice_viewer
                        .ipc_connection
                        .send(&IpcRequest {
                            request_id,
                            request_kind: IpcRequestKind::SubmitCommand {
                                command_envelope: Box::new(CommandEnvelope::from_parts(
                                    CommandId::new(),
                                    CommandSource::from_key_binding(alice_client_id),
                                    alice_command,
                                )),
                                fired_action: None,
                            },
                        })
                        .expect("send Alice's command");
                }
                let is_shown_to_alice = independence_case.is_shown_to_alice;
                let (mut alice_viewer, _) = read_session_frames_until_painted_frame(
                    alice_viewer,
                    move |received_painted_frame| {
                        is_shown_to_alice(&layout, received_painted_frame)
                    },
                );
                send_viewer_local_input_mode(&mut alice_viewer.ipc_connection, 5, true);
                let (bob_viewer, bob_frames_after) =
                    read_session_frames_until_painted_frame(bob_viewer, is_alice_typing_in_no_pane);
                assert_eq!(
                    describe_painted_view(get_last_painted_frame(&bob_frames_after)),
                    describe_painted_view(&bob_frame_before),
                    "{case_name}",
                );

                (
                    vec![
                        alice_viewer.ipc_connection,
                        bob_viewer.ipc_connection,
                        caller_connection,
                    ],
                    (),
                )
            },
        );
    }
}

/// The session, client, panes and tab each command of
/// [`every_command_from_a_remote_person_gets_the_answer_a_local_client_gets`]
/// names. The first tab holds the seeded pane beside `right_pane_id`, whose
/// program printed [`push_history_lines`]. One floating pane
/// holds `floating_pane_id`. The one attached client views the first tab and
/// shows the floating pane minimized. `destination_session_id` names a
/// session that no server serves.
#[derive(Debug, Clone, Copy)]
struct AccessTargets {
    destination_session_id: SessionId,
    seeded_pane_id: PaneId,
    right_pane_id: PaneId,
    floating_pane_id: PaneId,
    second_tab_id: TabId,
}

/// Replace each id string inside `answer_value` with `id-<n>`, where `n`
/// counts the distinct ids in the order they first appear. `index_by_id`
/// holds the ids replaced so far.
fn replace_ids_by_first_appearance(
    answer_value: &mut serde_json::Value,
    index_by_id: &mut HashMap<Uuid, usize>,
) {
    match answer_value {
        serde_json::Value::String(answer_text) => {
            if let Ok(answer_id) = Uuid::parse_str(answer_text) {
                let next_index = index_by_id.len();
                let id_index = *index_by_id.entry(answer_id).or_insert(next_index);
                *answer_text = format!("id-{id_index}");
            }
        }
        serde_json::Value::Array(answer_values) => {
            for nested_value in answer_values {
                replace_ids_by_first_appearance(nested_value, index_by_id);
            }
        }
        serde_json::Value::Object(answer_fields) => {
            for nested_value in answer_fields.values_mut() {
                replace_ids_by_first_appearance(nested_value, index_by_id);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// Build the panes and tab of [`AccessTargets`] in a fresh session, attach
/// one client on a connection whose Hello carries `granted_identity` (the
/// local Hello when `None`), and run the command `build_command` builds over
/// a second such connection. Returns the command's variant name and the
/// session's answer as JSON with its ids replaced
/// ([`replace_ids_by_first_appearance`]): the session, the client, the
/// seeded tab, the seeded pane, `right_pane_id`, `floating_pane_id`, the
/// second tab and `destination_session_id` take `id-0` to `id-7` in that
/// order, and every other id takes the next number at its first appearance.
fn run_command_in_fresh_session(
    session_label: &str,
    granted_identity: Option<&'static str>,
    build_command: fn(&AccessTargets) -> Command,
) -> (&'static str, serde_json::Value) {
    let (_, _, command_answer) = serve_test_session(
        session_label,
        move |runtime_directory, session_id, fake_pty_backend| {
            let open_connection = || match granted_identity {
                Some(granted_identity) => {
                    open_remote_session_connection(&runtime_directory, session_id, granted_identity)
                }
                None => open_session_connection(&runtime_directory, session_id),
            };
            let mut viewer_connection = open_connection();
            let viewer_client_id =
                attach_test_client(&mut viewer_connection, 2, LARGE_VIEWPORT_SIZE).client_id;
            let (seeded_tab_id, seeded_pane_id) =
                get_seeded_tab_and_pane_ids(&runtime_directory, session_id);
            let mut caller_connection = open_connection();
            let right_pane_id = create_right_split_pane(
                &mut caller_connection,
                session_id,
                seeded_pane_id,
                viewer_client_id,
                2,
            );
            let floating_pane_id = create_pane_for_client(
                &mut caller_connection,
                session_id,
                viewer_client_id,
                NewPanePlacement::Floating {
                    size: None,
                    top_left_cell: None,
                    is_pinned: false,
                },
                3,
            );
            let (second_tab_id, _) = get_created_tab_and_pane_ids(&submit_test_command(
                &mut caller_connection,
                session_id,
                Command::NewTab(NewTabArgs {
                    working_directory: None,
                    client_id: Some(viewer_client_id),
                }),
                4,
            ));
            submit_test_command(
                &mut caller_connection,
                session_id,
                Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Id(seeded_tab_id),
                    client_id: Some(viewer_client_id),
                }),
                5,
            );
            push_history_lines(&fake_pty_backend, right_pane_id);

            let destination_session_id = SessionId::new();
            let command = build_command(&AccessTargets {
                destination_session_id,
                seeded_pane_id,
                right_pane_id,
                floating_pane_id,
                second_tab_id,
            });
            // The variant name of `command`. The match lists every `Command`
            // variant and has no catch-all arm.
            let command_variant_name = match &command {
                Command::NewPane(_) => "NewPane",
                Command::ClosePane(_) => "ClosePane",
                Command::ResizePane(_) => "ResizePane",
                Command::FocusPane(_) => "FocusPane",
                Command::NewTab(_) => "NewTab",
                Command::CloseTab(_) => "CloseTab",
                Command::FocusTab(_) => "FocusTab",
                Command::WriteToPane(_) => "WriteToPane",
                Command::ToggleLockMode(_) => "ToggleLockMode",
                Command::SetLockMode(_) => "SetLockMode",
                Command::ToggleMouseSelect => "ToggleMouseSelect",
                Command::Visual(_) => "Visual",
                Command::TogglePaneFullscreen => "TogglePaneFullscreen",
                Command::MoveTab(_) => "MoveTab",
                Command::MovePane(_) => "MovePane",
                Command::PlacePane(_) => "PlacePane",
                Command::TogglePaneFloating => "TogglePaneFloating",
                Command::DockFloatingPane(_) => "DockFloatingPane",
                Command::ScrollPane(_) => "ScrollPane",
                Command::MoveFloatingPane(_) => "MoveFloatingPane",
                Command::SetFloatingPanePinned(_) => "SetFloatingPanePinned",
                Command::ToggleFloatingPanePinned(_) => "ToggleFloatingPanePinned",
                Command::SetFloatingPaneMinimized(_) => "SetFloatingPaneMinimized",
                Command::ToggleFloatingPaneMinimized(_) => "ToggleFloatingPaneMinimized",
                Command::SetAllFloatingPanesMinimized(_) => "SetAllFloatingPanesMinimized",
                Command::ToggleAllFloatingPanesMinimized => "ToggleAllFloatingPanesMinimized",
                Command::CloseFloatingPane(_) => "CloseFloatingPane",
                Command::Quit(_) => "Quit",
                Command::KillSession => "KillSession",
                Command::Detach(_) => "Detach",
                Command::DetachAll => "DetachAll",
                Command::SwitchSession(_) => "SwitchSession",
            };
            let answer_result = submit_test_command_for_answer(
                &mut caller_connection,
                session_id,
                None,
                command,
                6,
            );
            let mut index_by_id = HashMap::new();
            let mut known_ids = serde_json::json!([
                session_id,
                viewer_client_id,
                seeded_tab_id,
                seeded_pane_id,
                right_pane_id,
                floating_pane_id,
                second_tab_id,
                destination_session_id,
            ]);
            replace_ids_by_first_appearance(&mut known_ids, &mut index_by_id);
            let mut answer_value =
                serde_json::to_value(&answer_result).expect("an answer encodes as JSON");
            replace_ids_by_first_appearance(&mut answer_value, &mut index_by_id);
            (
                vec![viewer_connection, caller_connection],
                (command_variant_name, answer_value),
            )
        },
    );
    command_answer
}

/// Every attached client can do everything any other attached client can do.
/// For one command of each [`Command`] variant, two sessions built the same
/// way give the same answer, ids aside: one where the client and the caller
/// connect from this machine, and one where both connect through the router
/// for the remote person `bob`. Every command applies, except
/// `Command::Visual` and `Command::ToggleFloatingPaneMinimized`, which no
/// `koshi` call may send.
#[test]
fn every_command_from_a_remote_person_gets_the_answer_a_local_client_gets() {
    let access_cases: [fn(&AccessTargets) -> Command; 32] = [
        |targets| {
            Command::NewPane(NewPaneArgs {
                placement: NewPanePlacement::Split {
                    source_pane_id: Some(targets.seeded_pane_id),
                    tab_id: None,
                    direction: Direction::Down,
                },
                working_directory: None,
                spawn_spec: None,
                client_id: None,
            })
        },
        |targets| {
            Command::ClosePane(ClosePaneArgs {
                pane_id: Some(targets.right_pane_id),
                should_force_close: true,
                should_kill_process_tree: false,
            })
        },
        |targets| {
            Command::ResizePane(ResizePaneArgs {
                pane_id: Some(targets.seeded_pane_id),
                direction: Direction::Right,
                resize_amount_cells: 2,
            })
        },
        |targets| {
            Command::FocusPane(FocusPaneArgs {
                focus_target: FocusTarget::Pane(targets.seeded_pane_id),
                client_id: None,
            })
        },
        |_| {
            Command::NewTab(NewTabArgs {
                working_directory: None,
                client_id: None,
            })
        },
        |targets| {
            Command::CloseTab(CloseTabArgs {
                tab_id: Some(targets.second_tab_id),
                should_force_close: true,
                should_kill_process_tree: false,
            })
        },
        |targets| {
            Command::FocusTab(FocusTabArgs {
                focus_target: TabTarget::Id(targets.second_tab_id),
                client_id: None,
            })
        },
        |targets| {
            Command::WriteToPane(WriteToPaneArgs {
                pane_id: Some(targets.right_pane_id),
                pane_input_bytes: b"ls\r".to_vec(),
            })
        },
        |_| Command::ToggleLockMode(ToggleLockModeArgs { client_id: None }),
        |_| {
            Command::SetLockMode(LockModeArgs {
                is_locked: true,
                client_id: None,
            })
        },
        |_| Command::ToggleMouseSelect,
        |targets| {
            Command::Visual(VisualCommand::ClearSelection(ClearSelectionArgs {
                pane_id: targets.right_pane_id,
            }))
        },
        |_| Command::TogglePaneFullscreen,
        |targets| {
            Command::MoveTab(MoveTabArgs {
                tab_id: Some(targets.second_tab_id),
                target_tab_index: 0,
            })
        },
        |targets| {
            Command::MovePane(MovePaneArgs {
                pane_id: Some(targets.right_pane_id),
                direction: Direction::Left,
            })
        },
        |targets| {
            Command::PlacePane(PlacePaneArgs {
                source_pane_id: Some(targets.right_pane_id),
                placement_target: PanePlacementTarget::Tiled(TiledPlacementTarget::Swap {
                    target_pane_id: targets.seeded_pane_id,
                }),
            })
        },
        |_| Command::TogglePaneFloating,
        |targets| {
            Command::DockFloatingPane(DockFloatingPaneArgs {
                pane_id: Some(targets.floating_pane_id),
                placement_target: None,
            })
        },
        |targets| {
            Command::ScrollPane(ScrollPaneArgs {
                pane_id: Some(targets.right_pane_id),
                scroll_line_count: 5,
            })
        },
        |targets| {
            Command::MoveFloatingPane(MoveFloatingPaneArgs {
                pane_id: targets.floating_pane_id,
                top_left_cell: Point { column: 5, row: 2 },
            })
        },
        |targets| {
            Command::SetFloatingPanePinned(SetFloatingPanePinnedArgs {
                pane_id: Some(targets.floating_pane_id),
                is_pinned: true,
            })
        },
        |targets| {
            Command::ToggleFloatingPanePinned(ToggleFloatingPanePinnedArgs {
                pane_id: Some(targets.floating_pane_id),
            })
        },
        |targets| {
            Command::SetFloatingPaneMinimized(SetFloatingPaneMinimizedArgs {
                pane_id: Some(targets.floating_pane_id),
                is_minimized: false,
            })
        },
        |targets| {
            Command::ToggleFloatingPaneMinimized(ToggleFloatingPaneMinimizedArgs {
                pane_id: Some(targets.floating_pane_id),
            })
        },
        |_| {
            Command::SetAllFloatingPanesMinimized(SetAllFloatingPanesMinimizedArgs {
                is_minimized: false,
            })
        },
        |_| Command::ToggleAllFloatingPanesMinimized,
        |targets| {
            Command::CloseFloatingPane(CloseFloatingPaneArgs {
                pane_id: Some(targets.floating_pane_id),
                should_force_close: true,
                should_kill_process_tree: false,
            })
        },
        |_| Command::Quit(QuitArgs { client_id: None }),
        |_| Command::KillSession,
        |_| Command::Detach(DetachArgs { client_id: None }),
        |_| Command::DetachAll,
        |targets| {
            Command::SwitchSession(SwitchSessionArgs {
                client_id: None,
                session_id: targets.destination_session_id,
                owner_user_id: None,
            })
        },
    ];

    let mut command_variant_names = HashSet::new();
    let mut refused_command_variant_names = Vec::new();
    for (case_index, build_command) in access_cases.into_iter().enumerate() {
        let (command_variant_name, local_answer) = run_command_in_fresh_session(
            &format!("access-{case_index}-local"),
            None,
            build_command,
        );
        let (_, remote_answer) = run_command_in_fresh_session(
            &format!("access-{case_index}-remote"),
            Some("bob"),
            build_command,
        );
        assert_eq!(remote_answer, local_answer, "{command_variant_name}");
        command_variant_names.insert(command_variant_name);
        if local_answer["CommandResult"].get("Rejected").is_some() {
            refused_command_variant_names.push(command_variant_name);
        }
    }
    assert_eq!(command_variant_names.len(), access_cases.len());
    assert_eq!(
        refused_command_variant_names,
        ["Visual", "ToggleFloatingPaneMinimized"]
    );
}
