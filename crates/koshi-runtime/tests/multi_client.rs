//! Integration tests for multiple clients attached to one session: what size
//! the tab they share gives its panes, how a client viewing another tab is
//! left out of that size, what the larger client is shown when the shared size
//! is smaller than its own terminal, whose lock mode a lock command changes,
//! which pane cell a mouse press names for the client that sent it, what a
//! client moving to another session leaves behind here, and when the last
//! client moving away closes the session it left.

mod common;

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{
    count_attached_clients, get_last_painted_frame, open_session_connection,
    read_session_frames_to_detached, read_session_frames_until, serve_test_session,
    start_test_session, submit_test_command, wait_for_client_count, TEST_WAIT_TIMEOUT_DURATION,
};
use koshi_config::layer::PartialKoshiConfig;
use koshi_core::command::{
    Command, DetachArgs, FocusTabArgs, LockModeArgs, NewTabArgs, SwitchSessionArgs, TabTarget,
};
use koshi_core::event::{Event, InputModeChanged, PtyResized};
use koshi_core::geometry::{PaneArea, Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::key::BindingModifierFlags;
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseInput, MouseKind, MouseTracking};
use koshi_core::process::PtySize;
use koshi_ipc::attach::AttachedSessionStructureSnapshot;
use koshi_ipc::event::SessionEvent;
use koshi_ipc::frame::FrameSlot;
use koshi_ipc::protocol::{IpcRequest, IpcRequestKind, IpcResponse, IpcResult, WireMouseAction};
use koshi_ipc::transport::Connection;
use koshi_runtime::server::Server;
use koshi_test_support::fake_pty::FakePtyBackend;

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

/// Attach on `ipc_connection` reporting `viewport_size` and no pane area, and return
/// what the reply carried. The IPC connection carries only the client's event
/// stream afterwards.
fn attach_test_client_with_viewport_size(
    ipc_connection: &mut Connection,
    request_id: u64,
    viewport_size: Size,
) -> (ClientId, SessionId, AttachedSessionStructureSnapshot) {
    let (client_id, session_id, session_structure, _) =
        attach_test_client_with_pane_area(ipc_connection, request_id, viewport_size, None);
    (client_id, session_id, session_structure)
}

/// Attach on `ipc_connection` reporting `viewport_size` and `pane_area`, and return what
/// the reply carried, including the pane area the reply echoed.
fn attach_test_client_with_pane_area(
    ipc_connection: &mut Connection,
    request_id: u64,
    viewport_size: Size,
    pane_area: Option<PaneArea>,
) -> (
    ClientId,
    SessionId,
    AttachedSessionStructureSnapshot,
    Option<PaneArea>,
) {
    ipc_connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Attach {
                viewport_size,
                resume_client_id: None,
                resume_token: None,
                pane_area,
                graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                cell_size: None,
            },
        })
        .expect("send attach");
    let attach_response: IpcResponse = ipc_connection.recv().expect("attach reply");
    assert_eq!(attach_response.request_id, Some(request_id));
    let IpcResult::Attached {
        client_id,
        session_id,
        session_structure,
        pane_area,
        ..
    } = attach_response.answer_result
    else {
        panic!(
            "expected an attach reply, got {:?}",
            attach_response.answer_result
        );
    };
    (client_id, session_id, session_structure, pane_area)
}

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
            let (_, _, session_structure) = attach_test_client_with_viewport_size(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            // The small client joins the same tab. It is narrower and shorter: it
            // takes both axes, and the pane's PTY shrinks on both.
            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_viewport_size(
                &mut small_client_connection,
                2,
                SMALL_VIEWPORT_SIZE,
            );

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
            let (_, _, session_structure, echoed_pane_area) = attach_test_client_with_pane_area(
                &mut client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                Some(reported_pane_area),
            );
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];
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
            let (_, _, session_structure, _) = attach_test_client_with_pane_area(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                None,
            );
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            let mut starving_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (_, _, _, echoed_pane_area) = attach_test_client_with_pane_area(
                &mut starving_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                Some(PaneArea::Starving),
            );
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
            let (_, _, session_structure, echoed_pane_area) = attach_test_client_with_pane_area(
                &mut starving_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                Some(PaneArea::Starving),
            );
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];
            assert_eq!(echoed_pane_area, Some(PaneArea::Starving));

            // The served loop renders between the two attaches, with the tab's
            // only viewer contributing no pane area.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            wait_for_client_count(&mut caller_connection, 1, 3);

            // The tab has no viewer that sizes it: it solves at 0x0, and the
            // starving client's own frames carry its pane suppressed.
            let (starving_client_connection, session_events) =
                read_session_frames_until(starving_client_connection, |session_event| {
                    matches!(session_event, SessionEvent::Painted { .. })
                });
            let painted_frame = get_last_painted_frame(&session_events);
            assert_eq!(
                painted_frame.session_snapshot.active_tab_snapshot.tab_size,
                Size {
                    column_count: 0,
                    row_count: 0
                },
            );
            assert!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
                    .is_every_pane_suppressed
            );
            assert_eq!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
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
            attach_test_client_with_pane_area(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                None,
            );
            wait_for_client_count(&mut caller_connection, 2, 1000);

            (
                vec![
                    caller_connection,
                    starving_client_connection,
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
            let (_, _, _, echoed_pane_area) = attach_test_client_with_pane_area(
                &mut client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
                None,
            );
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
            let (_, _, session_structure) = attach_test_client_with_viewport_size(
                &mut narrow_client_connection,
                2,
                NARROW_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            // The short client joins the same tab. It is wider but shorter: the
            // columns stay pinned by the narrow client and the rows drop to this
            // one's. Each axis takes its minimum from a different client.
            let mut short_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (short_client_id, _, _) = attach_test_client_with_viewport_size(
                &mut short_client_connection,
                2,
                SHORT_VIEWPORT_SIZE,
            );

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

            let (short_client_connection, session_events) =
                read_session_frames_to_detached(short_client_connection);
            assert_eq!(session_events.last(), Some(&SessionEvent::Detached));
            wait_for_client_count(&mut caller_connection, 1, 5);

            (
                vec![
                    caller_connection,
                    narrow_client_connection,
                    short_client_connection,
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
            let (large_client_id, _, session_structure) = attach_test_client_with_viewport_size(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );
            let first_tab_id = session_structure.tabs[0].tab_id;
            let first_pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

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
            let (small_client_id, _, _) = attach_test_client_with_viewport_size(
                &mut small_client_connection,
                2,
                SMALL_VIEWPORT_SIZE,
            );
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
    // The per-axis minimum of [`LARGE_VIEWPORT_SIZE`] and [`SMALL_VIEWPORT_SIZE`], as a pane
    // region.
    const SHARED_PANE_VIEWPORT_SIZE: Size = Size {
        column_count: 80,
        row_count: 28,
    };

    let (_server, _fake_pty_backend, ()) = serve_test_session(
        "letterbox",
        |runtime_directory, session_id, _fake_pty_backend| {
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (large_client_id, _, session_structure) = attach_test_client_with_viewport_size(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];
            let tab_id = session_structure.tabs[0].tab_id;

            // The small client joins the same tab and invalidates the layout. The
            // large client is sent a fresh frame at the size the two now share.
            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_viewport_size(
                &mut small_client_connection,
                2,
                SMALL_VIEWPORT_SIZE,
            );

            let (large_client_connection, session_events) =
                read_session_frames_until(large_client_connection, move |session_event| {
                    match session_event {
                        SessionEvent::Painted {
                            frame: painted_frame,
                        } => {
                            painted_frame.session_snapshot.active_tab_snapshot.tab_size
                                == SHARED_PANE_VIEWPORT_SIZE
                        }
                        _ => false,
                    }
                });
            let painted_frame = get_last_painted_frame(&session_events);

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
                painted_frame.session_snapshot.active_tab_snapshot.tab_size,
                SHARED_PANE_VIEWPORT_SIZE
            );

            // The tab holds one pane, solved at origin (0, 0) over the shared size,
            // with its content inside a 1-cell border.
            assert_eq!(
                painted_frame
                    .session_snapshot
                    .active_tab_snapshot
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
                    .is_every_pane_suppressed
            );

            (vec![large_client_connection, small_client_connection], ())
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
            let (large_client_id, _, _) = attach_test_client_with_viewport_size(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );

            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (small_client_id, _, _) = attach_test_client_with_viewport_size(
                &mut small_client_connection,
                2,
                SMALL_VIEWPORT_SIZE,
            );

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
            let (client_id, _, _) = attach_test_client_with_viewport_size(
                &mut client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );

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

/// Turn normal mouse tracking with SGR encoding on in `pane_id`, the way the
/// program running there does, and read `ipc_connection`'s stream until a painted
/// frame shows the pane asking for reports. From that frame on, a forwarded
/// event is written to the pane.
fn wait_for_mouse_tracking(
    fake_pty_backend: &FakePtyBackend,
    pane_id: PaneId,
    ipc_connection: Connection,
) -> Connection {
    fake_pty_backend
        .push_output(pane_id, b"\x1b[?1000h\x1b[?1006h".to_vec())
        .expect("the pane was spawned");
    let (ipc_connection, _) =
        read_session_frames_until(ipc_connection, move |session_event| match session_event {
            SessionEvent::Painted {
                frame: painted_frame,
            } => painted_frame.pane_snapshots.iter().any(|pane_snapshot| {
                pane_snapshot.pane_id == pane_id
                    && pane_snapshot.mouse_tracking == MouseTracking::Normal
            }),
            _ => false,
        });
    ipc_connection
}

/// Send one mouse round holding a single left press on `pane_id` at the client
/// cell `screen_point`, then read `ipc_connection`'s stream until that round
/// is answered.
fn send_mouse_press(
    mut ipc_connection: Connection,
    pane_id: PaneId,
    screen_point: Point,
    request_id: u64,
) -> Connection {
    ipc_connection
        .send(&IpcRequest {
            request_id,
            request_kind: IpcRequestKind::Mouse(vec![WireMouseAction::Forward {
                pane_id,
                mouse_input: MouseInput {
                    mouse_kind: MouseKind::Press(MouseButton::Left),
                    position: screen_point,
                    modifier_flags: BindingModifierFlags::NONE,
                },
            }]),
        })
        .expect("send mouse round");
    let (ipc_connection, _) =
        read_session_frames_until(ipc_connection, move |session_event| match session_event {
            SessionEvent::MouseAnswer {
                request_id: answered_request_id,
                mouse_answers: _,
            } => *answered_request_id == request_id,
            _ => false,
        });
    ipc_connection
}

#[test]
fn a_mouse_click_is_answered_against_the_clicking_clients_own_view() {
    // The one cell both clients press, each in its own terminal. The tab they
    // share is 80 by 28: the small client's 80x30 terminal holds it at (0, 1),
    // putting the pane's content at (1, 2), and the large client's 100x40
    // terminal centers it at (10, 6), putting the pane's content at (11, 7).
    // This cell is the pane's column 11, row 6 for the small client, and the
    // pane's column 1, row 1 for the large one.
    const SHARED_PANE_CELL_POSITION: Point = Point { column: 11, row: 7 };

    // A cell in the large client's own 100x40 terminal, past the right and bottom
    // edges of the pane's content there (columns 11 to 88, rows 7 to 32): the
    // letterbox margin around the shared tab. It is pulled to the nearest
    // content cell, the pane's column 78, row 26.
    const LETTERBOX_MARGIN_CELL_POSITION: Point = Point {
        column: 90,
        row: 35,
    };

    let (_server, fake_pty_backend, pane_id) = serve_test_session(
        "per-client-mouse",
        |runtime_directory, session_id, fake_pty_backend| {
            let mut large_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (_, _, session_structure) = attach_test_client_with_viewport_size(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            attach_test_client_with_viewport_size(
                &mut small_client_connection,
                2,
                SMALL_VIEWPORT_SIZE,
            );

            // The program in the pane asks for mouse reports. Until it has, a
            // forwarded event is written nowhere.
            let small_client_connection =
                wait_for_mouse_tracking(&fake_pty_backend, pane_id, small_client_connection);

            let small_client_connection = send_mouse_press(
                small_client_connection,
                pane_id,
                SHARED_PANE_CELL_POSITION,
                3,
            );
            let large_client_connection = send_mouse_press(
                large_client_connection,
                pane_id,
                SHARED_PANE_CELL_POSITION,
                3,
            );
            let large_client_connection = send_mouse_press(
                large_client_connection,
                pane_id,
                LETTERBOX_MARGIN_CELL_POSITION,
                4,
            );

            (
                vec![large_client_connection, small_client_connection],
                pane_id,
            )
        },
    );

    // Three reports, in the order the rounds ran. The same terminal cell names
    // a different pane cell for each client: each round is placed in the view
    // of the client that sent it.
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
            let (_, _, session_structure) = attach_test_client_with_viewport_size(
                &mut client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );
            let pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            // The client is the tab's only viewer. The pane's content starts at
            // column 1, row 2 of its terminal: one tabline row, then the pane's
            // 1-cell border.
            let client_connection =
                send_mouse_press(client_connection, pane_id, Point { column: 1, row: 2 }, 3);

            (vec![client_connection], pane_id)
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
    // The per-axis minimum of [`LARGE_VIEWPORT_SIZE`] and [`SMALL_VIEWPORT_SIZE`], as a pane
    // region: the size the tab holds while both clients view it.
    const SHARED_PANE_VIEWPORT_SIZE: Size = Size {
        column_count: 80,
        row_count: 28,
    };

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
                let (joined_client_id, joined_session_id, session_structure) =
                    attach_test_client_with_viewport_size(
                        &mut joining_connection,
                        2,
                        SMALL_VIEWPORT_SIZE,
                    );
                assert_eq!(joined_session_id, session_id);
                (
                    vec![joining_connection],
                    (
                        session_id,
                        joined_client_id,
                        session_structure.tabs[0].layout.list_leaf_pane_ids()[0],
                    ),
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
            let (_, _, session_structure) = attach_test_client_with_viewport_size(
                &mut large_client_connection,
                2,
                LARGE_VIEWPORT_SIZE,
            );
            let source_pane_id = session_structure.tabs[0].layout.list_leaf_pane_ids()[0];

            let mut small_client_connection =
                open_session_connection(&runtime_directory, session_id);
            let (small_client_id, _, _) = attach_test_client_with_viewport_size(
                &mut small_client_connection,
                2,
                SMALL_VIEWPORT_SIZE,
            );

            // Read the large client's stream past the shared size. The next frame
            // read after the move is one the move caused.
            let (large_client_connection, _) =
                read_session_frames_until(large_client_connection, |session_event| {
                    match session_event {
                        SessionEvent::Painted {
                            frame: painted_frame,
                        } => {
                            painted_frame.session_snapshot.active_tab_snapshot.tab_size
                                == SHARED_PANE_VIEWPORT_SIZE
                        }
                        _ => false,
                    }
                });

            // The move itself. It puts the other session on the moved client's own
            // queue, changes nothing here, and emits nothing.
            let mut caller_connection = open_session_connection(&runtime_directory, session_id);
            let emitted_events = submit_test_command(
                &mut caller_connection,
                session_id,
                Command::SwitchSession(SwitchSessionArgs {
                    client_id: Some(small_client_id),
                    session_id: destination_session_id,
                }),
                3,
            );
            assert_eq!(emitted_events, Vec::<Event>::new());

            // The moved client is told where to go on its event stream.
            let (small_client_connection, switch_session_events) =
                read_session_frames_until(small_client_connection, |session_event| {
                    matches!(session_event, SessionEvent::SwitchTo { .. })
                });
            assert_eq!(
                switch_session_events.last(),
                Some(&SessionEvent::SwitchTo {
                    session_id: destination_session_id,
                }),
            );

            // The client leaves by closing its connection, the same as a real
            // client does once it has read where to go.
            drop(small_client_connection);
            source_client_left_sender
                .send(())
                .expect("the other session is waiting");

            // The tab grows back to the client that stayed, over one pane inside a
            // 1-cell border: this session keeps serving that client.
            let (large_client_connection, large_client_session_events) =
                read_session_frames_until(large_client_connection, |session_event| {
                    match session_event {
                        SessionEvent::Painted {
                            frame: painted_frame,
                        } => {
                            painted_frame.session_snapshot.active_tab_snapshot.tab_size
                                == SINGLE_CLIENT_PANE_VIEWPORT_SIZE
                        }
                        _ => false,
                    }
                });
            assert_eq!(
                get_last_painted_frame(&large_client_session_events)
                    .session_snapshot
                    .active_tab_snapshot
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
                vec![caller_connection, large_client_connection],
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
/// Returns the server, so a test can read whether the session asked to close.
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
        server.resync_lagged();
        if server.poll_render(Instant::now()) {
            server.push_frames();
        }
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
    let (client_id, _, _) =
        attach_test_client_with_viewport_size(&mut client_connection, 2, SMALL_VIEWPORT_SIZE);

    let mut caller_connection = open_session_connection(&runtime_directory, session_id);
    let emitted_events = submit_test_command(
        &mut caller_connection,
        session_id,
        Command::SwitchSession(SwitchSessionArgs {
            client_id: Some(client_id),
            session_id: destination_session_id,
        }),
        3,
    );
    assert_eq!(emitted_events, Vec::<Event>::new());

    let (client_connection, session_events) =
        read_session_frames_until(client_connection, |session_event| {
            matches!(session_event, SessionEvent::SwitchTo { .. })
        });
    assert_eq!(
        session_events.last(),
        Some(&SessionEvent::SwitchTo {
            session_id: destination_session_id,
        }),
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
