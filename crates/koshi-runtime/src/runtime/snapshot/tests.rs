//! Tests for the render-snapshot builder: mapping live runtime state (session,
//! tabs, client, terminal grids) into a `RenderSnapshot`, the per-client
//! invariants the renderer relies on, and the engine-less and dead-pane paths.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::SystemTime;

use koshi_core::command::{GridPosition, Selection, SelectionKind};
use koshi_core::geometry::{PaneArea, Point, Rect, Size, SplitDirection};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::process::PtySize;
use koshi_layout::mode::LayoutMode;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::lifecycle::PaneLifecycleEvent;
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_renderer::snapshot::{
    ImagePlacementSnapshot, PlacementPaneSnapshot, PlacementTabSnapshot, PluginUiSnapshot,
    TabSnapshot,
};
use koshi_session::client::{Client, ClientOrigin, ClientRegistry};
use koshi_session::session::state::{Session, Tab};
use koshi_terminal::engine::TerminalEngine;
use koshi_terminal::graphics::{
    DecodedImage, GraphicsProtocol, ImageAction, ImageDisplay, ImageRecord,
};
use koshi_terminal::state::CursorShape;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

fn build_test_runtime() -> Server {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let (tx, inbox_rx) = mpsc::channel::<RuntimeEvent>();
    Server::from_runtime_parts(pty_backend, inbox_rx, tx.clone())
}

/// A session with one tab (single-pane layout), the pane registered, and one
/// client attached viewing that tab focused on the pane, reporting no pane
/// area.
fn build_session_with_client(viewport_size: Size) -> (Session, SessionId, TabId, PaneId, ClientId) {
    build_session_with_client_reporting(viewport_size, None)
}

/// [`build_session_with_client`] whose one client reports `pane_area`.
fn build_session_with_client_reporting(
    viewport_size: Size,
    pane_area: Option<PaneArea>,
) -> (Session, SessionId, TabId, PaneId, ClientId) {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();

    let mut session = Session::from_identity_and_client_registry(
        session_id,
        "s".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(pane_id, SystemTime::now()))
        .expect("unique pane id");
    session.tabs.insert(
        tab_id,
        Tab::from_root_pane(tab_id, "t".to_string(), 0, pane_id),
    );

    let mut client = Client::from_attachment(
        client_id,
        session_id,
        SystemTime::now(),
        viewport_size,
        pane_area,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);

    (session, session_id, tab_id, pane_id, client_id)
}

#[test]
fn build_snapshot_for_an_unknown_client_is_none() {
    let server = build_test_runtime();
    assert_eq!(server.build_snapshot(ClientId::new()), None);
}

#[test]
fn build_snapshot_is_none_when_the_clients_viewed_tab_is_gone() {
    let mut server = build_test_runtime();
    let (mut session, session_id, tab_id, _pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    // The client still names the tab it was viewing; the tab itself is gone.
    session.tabs.remove(&tab_id);
    server.session_by_id.insert(session_id, session);

    assert_eq!(server.build_snapshot(client_id), None);
}

#[test]
fn build_snapshot_maps_session_tab_and_client() {
    let mut server = build_test_runtime();
    let (session, session_id, tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);
    server.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }),
    );

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");

    // Session + client identity.
    assert_eq!(render_snapshot.session_snapshot.session_id, session_id);
    assert_eq!(render_snapshot.session_snapshot.session_name, "s");
    assert_eq!(render_snapshot.client_snapshot.client_id, client_id);
    assert_eq!(
        render_snapshot.client_snapshot.viewport_size,
        Size {
            column_count: 80,
            row_count: 24
        }
    );
    assert_eq!(
        render_snapshot.client_snapshot.focused_pane_id,
        Some(pane_id)
    );

    // The load-bearing per-client invariant: both name the same tab.
    assert_eq!(render_snapshot.client_snapshot.active_tab_id, tab_id);
    assert_eq!(
        render_snapshot.session_snapshot.active_tab_snapshot.tab_id,
        tab_id
    );

    // The solved tab: one visible pane slot.
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
        Size {
            column_count: 80,
            row_count: 22
        }
    );
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .pane_slots
            .len(),
        1
    );
    let pane_slot = &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0];
    assert_eq!(pane_slot.pane_id, pane_id);
    assert!(pane_slot.is_visible);
    // Full 80×24 client leaves an 80×22 pane region; border insets content.
    assert_eq!(
        pane_slot.outer_rect,
        Rect::from_size_at_origin(Size {
            column_count: 80,
            row_count: 22
        })
    );
    assert_eq!(
        pane_slot.content_rect,
        Some(Rect::from_origin_and_size(
            Point { column: 1, row: 1 },
            Size {
                column_count: 78,
                row_count: 20
            },
        ))
    );
    assert!(!pane_slot.is_suppressed);
    assert!(!pane_slot.is_dead);

    // Tab metadata: a single active tab at index 0.
    assert_eq!(render_snapshot.session_snapshot.tabs_metadata.len(), 1);
    assert_eq!(
        render_snapshot.session_snapshot.tabs_metadata[0].tab_id,
        tab_id
    );
    assert_eq!(
        render_snapshot.session_snapshot.tabs_metadata[0].tab_index,
        0
    );
    assert!(render_snapshot.session_snapshot.tabs_metadata[0].is_active);

    // One pane content entry, with a grid view (engine present).
    assert_eq!(render_snapshot.pane_snapshots.len(), 1);
    assert_eq!(render_snapshot.pane_snapshots[0].pane_id, pane_id);
    let grid_view = render_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .expect("grid view");
    assert_eq!(grid_view.view_row_offset, 0);
    assert_eq!(grid_view.grid.get_grid_dimensions(), (24, 80));

    // No plugin UI for a stock session.
    assert_eq!(
        render_snapshot.plugin_ui_snapshot,
        PluginUiSnapshot::default()
    );

    // No sequence pends before a prefix key is pressed.
}

#[test]
fn build_snapshot_carries_the_clients_lock_mode_and_mouse_select() {
    // The viewer resolves its own hint bar from these two, so a frame that
    // dropped either would paint the wrong labels.
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, _pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(render_snapshot.client_snapshot.lock_mode, LockMode::Normal);
    assert!(!render_snapshot.client_snapshot.is_mouse_selection_enabled);

    let client = server
        .session_by_id
        .get_mut(&session_id)
        .expect("session")
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client");
    client.update_lock_mode(LockMode::Locked);
    client.toggle_mouse_selection();

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(render_snapshot.client_snapshot.lock_mode, LockMode::Locked);
    assert!(render_snapshot.client_snapshot.is_mouse_selection_enabled);
}

#[test]
fn a_pane_without_a_terminal_engine_has_no_grid_view() {
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);
    // No engine inserted for pane_id.

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(render_snapshot.pane_snapshots.len(), 1);
    assert_eq!(render_snapshot.pane_snapshots[0].pane_id, pane_id);
    assert_eq!(render_snapshot.pane_snapshots[0].terminal_grid_view, None);
    assert!(!render_snapshot.pane_snapshots[0].cursor_snapshot.is_visible);
    assert_eq!(render_snapshot.pane_snapshots[0].pane_title, None);
}

#[test]
fn build_snapshot_carries_the_live_terminal_grid_and_cursor() {
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);
    let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    let _ = terminal_engine.process_pty_output(b"hi");
    server
        .terminal_engine_by_pane_id
        .insert(pane_id, terminal_engine);

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    let pane_snapshot = &render_snapshot.pane_snapshots[0];

    // Cursor advanced two columns, still visible.
    assert_eq!(pane_snapshot.cursor_snapshot.row_index, 0);
    assert_eq!(pane_snapshot.cursor_snapshot.column_index, 2);
    assert!(pane_snapshot.cursor_snapshot.is_visible);

    // The shared grid handle carries the printed cells at offset 0.
    let grid_view = pane_snapshot
        .terminal_grid_view
        .as_ref()
        .expect("grid view");
    assert_eq!(grid_view.view_row_offset, 0);
    assert_eq!(
        grid_view
            .grid
            .get_cell(0, 0)
            .map(|cell| cell.get_character()),
        Some('h')
    );
    assert_eq!(
        grid_view
            .grid
            .get_cell(0, 1)
            .map(|cell| cell.get_character()),
        Some('i')
    );

    // Mode/scrollback passthroughs read from the engine.
    assert!(!pane_snapshot.is_reverse_video);
    assert_eq!(pane_snapshot.scrollback_meta.retained_line_count, 0);
    assert!(!pane_snapshot.scrollback_meta.is_truncated);

    // A shell that never sent DECSCUSR has asked for no shape at all.
    assert_eq!(pane_snapshot.cursor_snapshot.shape, None);
    assert!(!pane_snapshot.cursor_snapshot.is_blinking);
}

#[test]
fn build_snapshot_carries_the_cursor_style_the_pane_asked_for() {
    // The bytes vim writes on entering insert mode: DECSCUSR "blinking bar".
    // They must reach the snapshot, which is what lets the app style the outer
    // terminal's cursor to match — a block in normal mode, a bar in insert.
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);
    server.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }),
    );

    server.handle_pty_output(pane_id, b"\x1b[5 q");
    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(
        render_snapshot.pane_snapshots[0].cursor_snapshot.shape,
        Some(CursorShape::Bar)
    );
    assert!(
        render_snapshot.pane_snapshots[0]
            .cursor_snapshot
            .is_blinking
    );

    // Leaving insert mode: back to a steady block.
    server.handle_pty_output(pane_id, b"\x1b[2 q");
    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(
        render_snapshot.pane_snapshots[0].cursor_snapshot.shape,
        Some(CursorShape::Block)
    );
    assert!(
        !render_snapshot.pane_snapshots[0]
            .cursor_snapshot
            .is_blinking
    );

    // vim exiting: `CSI 0 SP q` undoes its cursor, and the pane is back to
    // asking for nothing — the user's own terminal cursor stands again.
    server.handle_pty_output(pane_id, b"\x1b[0 q");
    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(
        render_snapshot.pane_snapshots[0].cursor_snapshot.shape,
        None
    );
    assert!(
        !render_snapshot.pane_snapshots[0]
            .cursor_snapshot
            .is_blinking
    );
}

#[test]
fn a_frozen_snapshot_keeps_its_grid_when_the_engine_writes_again() {
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);
    let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    let _ = terminal_engine.process_pty_output(b"A");
    server
        .terminal_engine_by_pane_id
        .insert(pane_id, terminal_engine);

    // Freeze frame 1 while cell (0, 0) holds 'A'.
    let first_render_snapshot = server.build_snapshot(client_id).expect("snapshot");

    // The engine writes more output after the freeze: CR home, then overwrite (0, 0).
    let _ = server
        .terminal_engine_by_pane_id
        .get_mut(&pane_id)
        .expect("terminal engine")
        .process_pty_output(b"\rB");

    // Copy-on-write: frame 1's shared grid still shows the pre-write glyph — the
    // later `active_grid_mut` cloned the buffer instead of mutating the frozen one.
    let grid1 = &first_render_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .expect("grid view")
        .grid;
    assert_eq!(
        grid1.get_cell(0, 0).map(|cell| cell.get_character()),
        Some('A')
    );

    // A fresh snapshot reflects the new write.
    let second_render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    let grid2 = &second_render_snapshot.pane_snapshots[0]
        .terminal_grid_view
        .as_ref()
        .expect("grid view")
        .grid;
    assert_eq!(
        grid2.get_cell(0, 0).map(|cell| cell.get_character()),
        Some('B')
    );
}

#[test]
fn building_a_snapshot_leaves_terminal_image_state_unchanged() {
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);

    let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    let _ =
        terminal_engine.process_pty_output(b"\x1b_Ga=T,f=32,s=1,v=1,c=1,r=1,C=1;/wAA/w==\x1b\\");
    let terminal_state_before_insert = terminal_engine.get_terminal_state().clone();
    server
        .terminal_engine_by_pane_id
        .insert(pane_id, terminal_engine);

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    let terminal_state_after_insert = server
        .terminal_engine_by_pane_id
        .get(&pane_id)
        .expect("terminal engine")
        .get_terminal_state();

    assert_eq!(terminal_state_after_insert, &terminal_state_before_insert);
    assert_eq!(
        render_snapshot.pane_snapshots[0]
            .image_placement_snapshots
            .len(),
        1
    );
    assert_eq!(
        render_snapshot.pane_snapshots[0].image_placement_snapshots[0].get_anchor_cell(),
        (0, 0)
    );
    assert_eq!(
        render_snapshot.pane_snapshots[0].image_placement_snapshots[0]
            .get_image_record()
            .expect("the local snapshot carries image content")
            .image
            .rgba_bytes,
        vec![255, 0, 0, 255]
    );
}

#[test]
fn native_image_fragments_keep_one_content_id_across_snapshot_placements() {
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);

    let image_bytes = b"\x1b]1337;File=inline=1;width=3;height=1;preserveAspectRatio=0:iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=\x07";
    let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    let mut image_output_bytes = image_bytes.to_vec();
    image_output_bytes.extend_from_slice(b"\x1b[1;2Hx");
    let _ = terminal_engine.process_pty_output(&image_output_bytes);

    let image_placement_fragments = terminal_engine
        .get_terminal_state()
        .list_image_placements_for_view(0);
    assert_eq!(image_placement_fragments.len(), 2);
    assert_ne!(
        image_placement_fragments[0].get_image_placement_id(),
        image_placement_fragments[1].get_image_placement_id()
    );
    assert_eq!(
        image_placement_fragments[0].get_image_content_id(),
        image_placement_fragments[1].get_image_content_id()
    );
    let image_content_id = image_placement_fragments[0].get_image_content_id();
    server
        .terminal_engine_by_pane_id
        .insert(pane_id, terminal_engine);

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    let image_placement_snapshots = &render_snapshot.pane_snapshots[0].image_placement_snapshots;
    assert_eq!(image_placement_snapshots.len(), 2);
    assert_ne!(
        image_placement_snapshots[0].get_placement_id(),
        image_placement_snapshots[1].get_placement_id()
    );
    assert_eq!(
        image_placement_snapshots[0].get_image_content_id(),
        image_content_id
    );
    assert_eq!(
        image_placement_snapshots[1].get_image_content_id(),
        image_content_id
    );
}

#[test]
fn placement_resource_count_shares_one_decoded_image_across_placements() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let image_record = Arc::new(ImageRecord {
        protocol: GraphicsProtocol::Iterm2,
        image: Arc::new(DecodedImage {
            pixel_width: 2,
            pixel_height: 2,
            rgba_bytes: vec![255; 16],
        }),
        animation: None,
        action: ImageAction::Display,
        display: ImageDisplay::default(),
        anchor: (0, 0),
    });
    let image_placement_snapshots = vec![
        ImagePlacementSnapshot::with_content_id(1, 7, Arc::clone(&image_record), (0, 0), 1, 1)
            .expect("first image placement"),
        ImagePlacementSnapshot::with_content_id(2, 7, image_record, (1, 0), 1, 1)
            .expect("second image placement"),
    ];
    let placement_tab_snapshot = PlacementTabSnapshot {
        layout_tree: LayoutNode::Pane(pane_id),
        tab_snapshot: TabSnapshot {
            tab_id,
            tab_name: "preview".to_string(),
            pane_slots: Vec::new(),
            effective_cell_size: Size {
                column_count: 2,
                row_count: 1,
            },
            stack_headers: Vec::new(),
            layout_mode: LayoutMode::Tiled,
            are_all_panes_suppressed: false,
            gap_cell_count: 0,
        },
        pane_snapshots: vec![PlacementPaneSnapshot {
            pane_id,
            terminal_grid_view: None,
            image_placement_snapshots,
        }],
    };

    assert_eq!(
        super::count_placement_snapshot_resources(&placement_tab_snapshot, None),
        (0, 2, 16)
    );
}

#[test]
fn placement_resource_count_distinguishes_same_local_id_from_different_images() {
    let pane_id = PaneId::new();
    let tab_id = TabId::new();
    let build_image_record = |byte: u8| {
        Arc::new(ImageRecord {
            protocol: GraphicsProtocol::Iterm2,
            image: Arc::new(DecodedImage {
                pixel_width: 2,
                pixel_height: 2,
                rgba_bytes: vec![byte; 16],
            }),
            animation: None,
            action: ImageAction::Display,
            display: ImageDisplay::default(),
            anchor: (0, 0),
        })
    };
    let first_image_record = build_image_record(0);
    let second_image_record = build_image_record(255);
    let image_placement_snapshots = vec![
        ImagePlacementSnapshot::with_content_id(1, 7, first_image_record, (0, 0), 1, 1)
            .expect("first image placement"),
        ImagePlacementSnapshot::with_content_id(2, 7, second_image_record, (1, 0), 1, 1)
            .expect("second image placement"),
    ];
    let placement_tab_snapshot = PlacementTabSnapshot {
        layout_tree: LayoutNode::Pane(pane_id),
        tab_snapshot: TabSnapshot {
            tab_id,
            tab_name: "preview".to_string(),
            pane_slots: Vec::new(),
            effective_cell_size: Size {
                column_count: 2,
                row_count: 1,
            },
            stack_headers: Vec::new(),
            layout_mode: LayoutMode::Tiled,
            are_all_panes_suppressed: false,
            gap_cell_count: 0,
        },
        pane_snapshots: vec![PlacementPaneSnapshot {
            pane_id,
            terminal_grid_view: None,
            image_placement_snapshots,
        }],
    };

    assert_eq!(
        super::count_placement_snapshot_resources(&placement_tab_snapshot, None),
        (0, 2, 32)
    );
}

#[test]
fn effective_size_is_the_min_viewport_across_clients_not_the_requesters() {
    let mut server = build_test_runtime();
    let (mut session, session_id, tab_id, pane_id, big_client) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });

    // A second client views the same tab at a smaller viewport.
    let small_client = ClientId::new();
    let mut client = Client::from_attachment(
        small_client,
        session_id,
        SystemTime::now(),
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);
    server.session_by_id.insert(session_id, session);

    let render_snapshot = server.build_snapshot(big_client).expect("snapshot");
    // The requesting client's own viewport is unchanged...
    assert_eq!(
        render_snapshot.client_snapshot.viewport_size,
        Size {
            column_count: 80,
            row_count: 24
        }
    );
    // ...but the tab is solved at the shared minimum, which the renderer letterboxes.
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
        Size {
            column_count: 40,
            row_count: 8
        }
    );
}

#[test]
fn placement_destination_size_includes_requesting_client_before_destination_viewers() {
    let mut server = build_test_runtime();
    let (mut session, session_id, source_tab_id, source_pane_id, requesting_client_id) =
        build_session_with_client_reporting(
            Size {
                column_count: 160,
                row_count: 50,
            },
            Some(PaneArea::Reported(Size {
                column_count: 80,
                row_count: 20,
            })),
        );
    let destination_tab_id = TabId::new();
    let destination_pane_id = PaneId::new();
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(
            destination_pane_id,
            SystemTime::now(),
        ))
        .expect("unique pane id");
    session.tabs.insert(
        destination_tab_id,
        Tab::from_root_pane(
            destination_tab_id,
            "destination".to_string(),
            1,
            destination_pane_id,
        ),
    );

    let destination_viewer_id = ClientId::new();
    let mut destination_viewer = Client::from_attachment(
        destination_viewer_id,
        session_id,
        SystemTime::now(),
        Size {
            column_count: 240,
            row_count: 60,
        },
        Some(PaneArea::Reported(Size {
            column_count: 120,
            row_count: 40,
        })),
        destination_tab_id,
        ClientOrigin::Local,
        "C-test-destination-viewer".to_string(),
        0,
    );
    destination_viewer.update_focused_pane(destination_tab_id, destination_pane_id);
    session.attach_client(destination_viewer);
    server.session_by_id.insert(session_id, session);
    server.terminal_engine_by_pane_id.insert(
        destination_pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 118,
            row_count: 38,
        }),
    );

    let placement_snapshot = server
        .build_placement_snapshot(requesting_client_id, source_pane_id, destination_tab_id)
        .expect("placement snapshot");

    assert_eq!(placement_snapshot.source_tab_id, source_tab_id);
    assert_eq!(
        placement_snapshot
            .destination_tab_snapshot
            .as_ref()
            .expect("cross-tab destination")
            .tab_snapshot
            .effective_cell_size,
        Size {
            column_count: 80,
            row_count: 20,
        }
    );
    assert_eq!(
        crate::runtime::frame::wire_placement_snapshot(&placement_snapshot).validate(),
        Ok(())
    );
}

#[test]
fn build_snapshot_for_a_starving_sole_viewer_suppresses_every_pane() {
    let mut server = build_test_runtime();
    let (mut session, session_id, tab_id, pane_id, client_id) = build_session_with_client_reporting(
        Size {
            column_count: 80,
            row_count: 24,
        },
        Some(PaneArea::Starving),
    );
    let second_pane = PaneId::new();
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(
            second_pane,
            SystemTime::now(),
        ))
        .expect("unique pane id");
    session
        .tabs
        .get_mut(&tab_id)
        .expect("tab")
        .update_layout(LayoutNode::Split(SplitNode::with_equal_weights(
            SplitDirection::Horizontal,
            vec![LayoutNode::Pane(pane_id), LayoutNode::Pane(second_pane)],
        )));
    server.session_by_id.insert(session_id, session);
    server.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }),
    );
    server.terminal_engine_by_pane_id.insert(
        second_pane,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }),
    );

    // The tab's only viewer contributes no pane area: the tab solves at 0x0,
    // every pane is suppressed, and the client still gets a frame.
    let render_snapshot = server.build_snapshot(client_id).expect("a frame");
    assert_eq!(
        render_snapshot.client_snapshot.viewport_size,
        Size {
            column_count: 80,
            row_count: 24
        }
    );
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
        Size {
            column_count: 0,
            row_count: 0
        }
    );
    assert!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .are_all_panes_suppressed
    );
    let slots: Vec<(PaneId, bool, bool, Option<Rect>)> = render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots
        .iter()
        .map(|slot| {
            (
                slot.pane_id,
                slot.is_suppressed,
                slot.is_visible,
                slot.content_rect,
            )
        })
        .collect();
    assert_eq!(
        slots,
        vec![
            (pane_id, true, false, None),
            (second_pane, true, false, None)
        ]
    );
}

#[test]
fn placement_snapshot_omits_cells_for_suppressed_panes() {
    let mut server = build_test_runtime();
    let (session, session_id, tab_id, pane_id, client_id) = build_session_with_client_reporting(
        Size {
            column_count: 80,
            row_count: 24,
        },
        Some(PaneArea::Starving),
    );
    server.session_by_id.insert(session_id, session);
    server.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }),
    );

    let placement_snapshot = server
        .build_placement_snapshot(client_id, pane_id, tab_id)
        .expect("placement snapshot");
    let pane_slot = &placement_snapshot
        .source_tab_snapshot
        .tab_snapshot
        .pane_slots[0];
    let pane_snapshot = &placement_snapshot.source_tab_snapshot.pane_snapshots[0];

    assert!(pane_slot.is_suppressed);
    assert!(!pane_slot.is_visible);
    assert_eq!(pane_snapshot.terminal_grid_view, None);
    assert!(pane_snapshot.image_placement_snapshots.is_empty());
}

#[test]
fn cross_tab_placement_snapshot_retains_a_suppressed_source_pane_for_transfer() {
    let mut server = build_test_runtime();
    let (mut session, session_id, source_tab_id, source_pane_id, client_id) =
        build_session_with_client_reporting(
            Size {
                column_count: 80,
                row_count: 24,
            },
            Some(PaneArea::Reported(Size {
                column_count: 80,
                row_count: 24,
            })),
        );
    let destination_tab_id = TabId::new();
    let destination_pane_id = PaneId::new();
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(
            destination_pane_id,
            SystemTime::now(),
        ))
        .expect("unique pane id");
    session.tabs.insert(
        destination_tab_id,
        Tab::from_root_pane(
            destination_tab_id,
            "destination".to_string(),
            1,
            destination_pane_id,
        ),
    );
    server.session_by_id.insert(session_id, session);
    server.terminal_engine_by_pane_id.insert(
        source_pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }),
    );
    server.terminal_engine_by_pane_id.insert(
        destination_pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 80,
            row_count: 24,
        }),
    );

    let placement_snapshot = server
        .build_placement_snapshot(client_id, source_pane_id, destination_tab_id)
        .expect("placement snapshot");
    let source_pane_slot = &placement_snapshot
        .source_tab_snapshot
        .tab_snapshot
        .pane_slots[0];
    let source_pane_snapshot = &placement_snapshot.source_tab_snapshot.pane_snapshots[0];

    assert!(source_pane_slot.is_suppressed);
    assert!(!source_pane_slot.is_visible);
    let source_grid_view = source_pane_snapshot
        .terminal_grid_view
        .as_ref()
        .expect("the moved pane needs content for the destination preview");
    assert_eq!(source_grid_view.grid.get_grid_dimensions(), (24, 80));
    assert_eq!(
        crate::runtime::frame::wire_placement_snapshot(&placement_snapshot).validate(),
        Ok(())
    );
    assert_eq!(placement_snapshot.source_tab_id, source_tab_id);
}

#[test]
fn build_snapshot_for_a_starving_viewer_solves_at_the_other_viewers_pane_area() {
    let mut server = build_test_runtime();
    let (mut session, session_id, tab_id, pane_id, starving_client) =
        build_session_with_client_reporting(
            Size {
                column_count: 80,
                row_count: 24,
            },
            Some(PaneArea::Starving),
        );

    // A second client views the same tab at a smaller viewport, reporting no
    // pane area of its own.
    let sizing_client = ClientId::new();
    let mut client = Client::from_attachment(
        sizing_client,
        session_id,
        SystemTime::now(),
        Size {
            column_count: 40,
            row_count: 10,
        },
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);
    server.session_by_id.insert(session_id, session);

    let render_snapshot = server.build_snapshot(starving_client).expect("snapshot");
    // The requesting client's own viewport is unchanged...
    assert_eq!(
        render_snapshot.client_snapshot.viewport_size,
        Size {
            column_count: 80,
            row_count: 24
        }
    );
    // ...and the tab solves at the one viewer that contributes a pane area.
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .effective_cell_size,
        Size {
            column_count: 40,
            row_count: 8
        }
    );
}

#[test]
fn an_exited_pane_is_marked_dead_but_stays_visible() {
    let mut server = build_test_runtime();
    let (mut session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    {
        let pane_record = session
            .panes
            .get_pane_record_mut_by_id(pane_id)
            .expect("pane record");
        let _ = pane_record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
        let _ = pane_record.update_lifecycle(PaneLifecycleEvent::ProcessExited {
            exit_code: Some(0),
            exited_at: SystemTime::now(),
        });
    }
    server.session_by_id.insert(session_id, session);

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    let pane_slot = &render_snapshot
        .session_snapshot
        .active_tab_snapshot
        .pane_slots[0];
    assert!(pane_slot.is_dead);
    // `dead` is orthogonal to visibility: an exited pane stays laid out.
    assert!(pane_slot.is_visible);
}

#[test]
fn tabs_metadata_covers_every_tab_in_index_order_with_the_viewed_tab_active() {
    let mut server = build_test_runtime();
    let (mut session, session_id, first_tab_id, _pane_id, client_id) =
        build_session_with_client(Size {
            column_count: 80,
            row_count: 24,
        });

    // A second tab the client is not viewing.
    let second_tab_id = TabId::new();
    let second_pane_id = PaneId::new();
    session
        .panes
        .register_pane_record(PaneRecord::from_terminal_pane(
            second_pane_id,
            SystemTime::now(),
        ))
        .expect("unique pane id");
    session.tabs.insert(
        second_tab_id,
        Tab::from_root_pane(second_tab_id, "t2".to_string(), 1, second_pane_id),
    );
    server.session_by_id.insert(session_id, session);

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(render_snapshot.session_snapshot.tabs_metadata.len(), 2);
    assert_eq!(
        render_snapshot.session_snapshot.tabs_metadata[0].tab_index,
        0
    );
    assert_eq!(
        render_snapshot.session_snapshot.tabs_metadata[1].tab_index,
        1
    );

    // Only the client's viewed tab is active.
    let active_tab_ids: Vec<TabId> = render_snapshot
        .session_snapshot
        .tabs_metadata
        .iter()
        .filter(|meta| meta.is_active)
        .map(|meta| meta.tab_id)
        .collect();
    assert_eq!(active_tab_ids, vec![first_tab_id]);
    assert_eq!(
        render_snapshot.session_snapshot.active_tab_snapshot.tab_id,
        first_tab_id
    );
}

#[test]
fn tabs_metadata_is_ordered_by_index_not_by_tab_id() {
    let mut server = build_test_runtime();
    let (mut session, session_id, first_tab_id, _pane_id, client_id) =
        build_session_with_client(Size {
            column_count: 80,
            row_count: 24,
        });

    // Two more tabs, added with the higher index first. `session.tabs` is keyed
    // by tab id, so map order says nothing about bar order.
    let add_session_tab = |session: &mut Session, tab_index: usize| {
        let tab_id = TabId::new();
        let pane_id = PaneId::new();
        session
            .panes
            .register_pane_record(PaneRecord::from_terminal_pane(pane_id, SystemTime::now()))
            .expect("unique pane id");
        session.tabs.insert(
            tab_id,
            Tab::from_root_pane(tab_id, format!("t{tab_index}"), tab_index, pane_id),
        );
        tab_id
    };
    let third_tab_id = add_session_tab(&mut session, 3);
    let second_tab_id = add_session_tab(&mut session, 1);
    server.session_by_id.insert(session_id, session);

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");

    let tab_metadata_rows: Vec<(TabId, usize, bool)> = render_snapshot
        .session_snapshot
        .tabs_metadata
        .iter()
        .map(|meta| (meta.tab_id, meta.tab_index, meta.is_active))
        .collect();
    assert_eq!(
        tab_metadata_rows,
        vec![
            (first_tab_id, 0, true),
            (second_tab_id, 1, false),
            (third_tab_id, 3, false),
        ]
    );
}

#[test]
fn session_lookups_return_none_for_ids_no_session_holds() {
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, _pane_id, _client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);

    assert_eq!(
        server
            .get_session_for_client(ClientId::new())
            .map(|session| session.session_id),
        None
    );
    assert_eq!(
        server
            .get_session_for_pane(PaneId::new())
            .map(|session| session.session_id),
        None
    );
    assert_eq!(
        server
            .get_session_for_client_mut(ClientId::new())
            .map(|session| session.session_id),
        None
    );
    assert_eq!(
        server
            .get_session_for_pane_mut(PaneId::new())
            .map(|session| session.session_id),
        None
    );
}

#[test]
fn session_lookups_pick_the_owner_when_the_server_holds_several_sessions() {
    let mut server = build_test_runtime();
    let (first_session, first_session_id, _first_tab_id, first_pane_id, first_client_id) =
        build_session_with_client(Size {
            column_count: 80,
            row_count: 24,
        });
    let (second_session, second_session_id, _second_tab_id, second_pane_id, second_client_id) =
        build_session_with_client(Size {
            column_count: 80,
            row_count: 24,
        });
    server.session_by_id.insert(first_session_id, first_session);
    server
        .session_by_id
        .insert(second_session_id, second_session);

    assert_eq!(
        server
            .get_session_for_client(first_client_id)
            .map(|session| session.session_id),
        Some(first_session_id)
    );
    assert_eq!(
        server
            .get_session_for_client(second_client_id)
            .map(|session| session.session_id),
        Some(second_session_id)
    );
    assert_eq!(
        server
            .get_session_for_pane(first_pane_id)
            .map(|session| session.session_id),
        Some(first_session_id)
    );
    assert_eq!(
        server
            .get_session_for_pane(second_pane_id)
            .map(|session| session.session_id),
        Some(second_session_id)
    );

    // The mutable twins resolve the same way.
    assert_eq!(
        server
            .get_session_for_client_mut(second_client_id)
            .map(|session| session.session_id),
        Some(second_session_id)
    );
    assert_eq!(
        server
            .get_session_for_pane_mut(first_pane_id)
            .map(|session| session.session_id),
        Some(first_session_id)
    );
}

#[test]
fn snapshot_follows_live_output_when_the_client_has_not_scrolled() {
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);
    server.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 8,
            row_count: 1,
        }),
    );
    server.handle_pty_output(pane_id, b"\n\n\n"); // three retained lines

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    let pane_snapshot = render_snapshot
        .pane_snapshots
        .iter()
        .find(|pane_snapshot| pane_snapshot.pane_id == pane_id)
        .expect("pane");
    assert_eq!(
        pane_snapshot
            .terminal_grid_view
            .as_ref()
            .unwrap()
            .view_row_offset,
        0
    );
    assert_eq!(pane_snapshot.scrollback_meta.retained_line_count, 3);
}

#[test]
fn snapshot_carries_the_clients_scrolled_back_offset() {
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);
    server.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 8,
            row_count: 1,
        }),
    );
    server.handle_pty_output(pane_id, b"\n\n\n");
    server.scroll_up(client_id, pane_id, 2);

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    let pane_snapshot = render_snapshot
        .pane_snapshots
        .iter()
        .find(|pane_snapshot| pane_snapshot.pane_id == pane_id)
        .expect("pane");
    // The scrolled offset reaches the renderer as the view offset.
    assert_eq!(
        pane_snapshot
            .terminal_grid_view
            .as_ref()
            .unwrap()
            .view_row_offset,
        2
    );
    assert_eq!(pane_snapshot.scrollback_meta.retained_line_count, 3);
}

#[test]
fn snapshot_reports_a_live_offset_for_a_scrolled_client_on_the_alternate_screen() {
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);
    server.terminal_engine_by_pane_id.insert(
        pane_id,
        TerminalEngine::from_pty_size(PtySize {
            column_count: 8,
            row_count: 1,
        }),
    );
    server.handle_pty_output(pane_id, b"\n\n\n");
    server.scroll_up(client_id, pane_id, 2);
    server.handle_pty_output(pane_id, b"\x1b[?1049h"); // enter the alternate screen

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    let pane_snapshot = render_snapshot
        .pane_snapshots
        .iter()
        .find(|pane_snapshot| pane_snapshot.pane_id == pane_id)
        .expect("pane");
    // The alternate screen keeps no scrollback: the client's stored offset does
    // not apply there, so the renderer sees effective offset 0.
    assert_eq!(
        pane_snapshot
            .terminal_grid_view
            .as_ref()
            .unwrap()
            .view_row_offset,
        0
    );
}

#[test]
fn shorten_home_replaces_the_prefix_only_on_a_path_boundary() {
    use super::shorten_home_path;
    use std::path::Path;
    let home_path = Some("/Users/ab");
    assert_eq!(shorten_home_path(Path::new("/Users/ab"), home_path), "~");
    assert_eq!(
        shorten_home_path(Path::new("/Users/ab/koshi"), home_path),
        "~/koshi"
    );
    // A sibling directory sharing the prefix text is NOT under home.
    assert_eq!(
        shorten_home_path(Path::new("/Users/ab2/x"), home_path),
        "/Users/ab2/x"
    );
    assert_eq!(shorten_home_path(Path::new("/tmp"), None), "/tmp");
}

// ============================================================================
// Highlight resolution: absolute line numbers to the rows a frame shows
// ============================================================================

/// A runtime with one client and a pane whose terminal has received terminal input bytes.
fn build_runtime_with_terminal_input(terminal_input_bytes: &[u8]) -> (Server, PaneId, ClientId) {
    let mut server = build_test_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) = build_session_with_client(Size {
        column_count: 80,
        row_count: 24,
    });
    server.session_by_id.insert(session_id, session);
    let mut terminal_engine = TerminalEngine::from_pty_size(PtySize {
        column_count: 80,
        row_count: 24,
    });
    let _ = terminal_engine.process_pty_output(terminal_input_bytes);
    server
        .terminal_engine_by_pane_id
        .insert(pane_id, terminal_engine);
    (server, pane_id, client_id)
}

/// The highlight rows the frame carries for the client's only pane.
fn get_selection_row_spans(server: &Server, client_id: ClientId) -> Option<Vec<(u16, u16, u16)>> {
    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    render_snapshot.pane_snapshots[0]
        .selection_spans
        .as_ref()
        .map(|selection_spans| selection_spans.row_spans.clone())
}

fn build_character_selection(anchor: GridPosition, cursor: GridPosition) -> Selection {
    Selection {
        selection_kind: SelectionKind::Character,
        anchor,
        cursor,
    }
}

#[test]
fn a_pane_with_no_highlight_carries_none() {
    let (server, _pane_id, client_id) = build_runtime_with_terminal_input(b"hello");
    assert_eq!(get_selection_row_spans(&server, client_id), None);
}

#[test]
fn a_highlight_on_one_row_is_one_span() {
    let (mut server, pane_id, client_id) = build_runtime_with_terminal_input(b"hello world");
    server
        .get_client_mut(client_id)
        .expect("client")
        .set_selection(
            pane_id,
            build_character_selection(
                GridPosition {
                    row_index: 0,
                    column_index: 6,
                },
                GridPosition {
                    row_index: 0,
                    column_index: 10,
                },
            ),
        );

    assert_eq!(
        get_selection_row_spans(&server, client_id),
        Some(vec![(0, 6, 10)])
    );
}

#[test]
fn a_highlight_over_three_rows_runs_with_the_text() {
    let (mut server, pane_id, client_id) = build_runtime_with_terminal_input(b"a\r\nb\r\nc\r\nd");
    // From column 12 of row 1 to column 33 of row 3: the first row runs to its
    // end, the middle row is whole, the last stops at its column.
    server
        .get_client_mut(client_id)
        .expect("client")
        .set_selection(
            pane_id,
            build_character_selection(
                GridPosition {
                    row_index: 1,
                    column_index: 12,
                },
                GridPosition {
                    row_index: 3,
                    column_index: 33,
                },
            ),
        );

    assert_eq!(
        get_selection_row_spans(&server, client_id),
        Some(vec![(1, 12, 79), (2, 0, 79), (3, 0, 33)])
    );
}

#[test]
fn a_block_highlight_is_the_same_columns_on_every_row() {
    let (mut server, pane_id, client_id) = build_runtime_with_terminal_input(b"a\r\nb\r\nc");
    server
        .get_client_mut(client_id)
        .expect("client")
        .set_selection(
            pane_id,
            Selection {
                selection_kind: SelectionKind::Block,
                anchor: GridPosition {
                    row_index: 0,
                    column_index: 4,
                },
                cursor: GridPosition {
                    row_index: 2,
                    column_index: 9,
                },
            },
        );

    assert_eq!(
        get_selection_row_spans(&server, client_id),
        Some(vec![(0, 4, 9), (1, 4, 9), (2, 4, 9)]),
        "a rectangle, not a run of text"
    );
}

#[test]
fn a_block_dragged_leftward_still_covers_the_columns_between() {
    let (mut server, pane_id, client_id) = build_runtime_with_terminal_input(b"a\r\nb");
    // The anchor's column is to the RIGHT of the cursor's.
    server
        .get_client_mut(client_id)
        .expect("client")
        .set_selection(
            pane_id,
            Selection {
                selection_kind: SelectionKind::Block,
                anchor: GridPosition {
                    row_index: 0,
                    column_index: 9,
                },
                cursor: GridPosition {
                    row_index: 1,
                    column_index: 4,
                },
            },
        );

    assert_eq!(
        get_selection_row_spans(&server, client_id),
        Some(vec![(0, 4, 9), (1, 4, 9)])
    );
}

#[test]
fn a_highlight_ending_on_a_wide_glyph_covers_its_whole_cell() {
    // `a世b`: the wide glyph is at column 1 and its blank half at column 2. A
    // selection ending on the glyph reaches its left column, and the renderer
    // paints the 2-wide glyph from there while skipping the width-0 half — so
    // the highlight covers the whole glyph and can never land on half of one.
    let (mut server, pane_id, client_id) = build_runtime_with_terminal_input("a世b".as_bytes());
    server
        .get_client_mut(client_id)
        .expect("client")
        .set_selection(
            pane_id,
            build_character_selection(
                GridPosition {
                    row_index: 0,
                    column_index: 0,
                },
                GridPosition {
                    row_index: 0,
                    column_index: 1,
                },
            ),
        );

    assert_eq!(
        get_selection_row_spans(&server, client_id),
        Some(vec![(0, 0, 1)])
    );
}

#[test]
fn word_and_line_highlights_run_with_the_text_like_a_character_one() {
    for selection_kind in [SelectionKind::Word, SelectionKind::Line] {
        let (mut server, pane_id, client_id) =
            build_runtime_with_terminal_input(b"a\r\nb\r\nc\r\nd");
        server
            .get_client_mut(client_id)
            .expect("client")
            .set_selection(
                pane_id,
                Selection {
                    selection_kind,
                    anchor: GridPosition {
                        row_index: 1,
                        column_index: 12,
                    },
                    cursor: GridPosition {
                        row_index: 3,
                        column_index: 33,
                    },
                },
            );

        assert_eq!(
            get_selection_row_spans(&server, client_id),
            Some(vec![(1, 12, 79), (2, 0, 79), (3, 0, 33)]),
            "{selection_kind:?} resolves to the same rows as a character highlight"
        );
    }
}

#[test]
fn a_highlight_below_the_bottom_of_the_view_is_not_drawn() {
    // Five characters through a 24-row screen: nothing has scrolled off, so the
    // frame shows absolute lines 0..=23. Line 30 is below all of them.
    let (mut server, pane_id, client_id) = build_runtime_with_terminal_input(b"hello");
    server
        .get_client_mut(client_id)
        .expect("client")
        .set_selection(
            pane_id,
            build_character_selection(
                GridPosition {
                    row_index: 30,
                    column_index: 0,
                },
                GridPosition {
                    row_index: 30,
                    column_index: 4,
                },
            ),
        );

    assert_eq!(
        get_selection_row_spans(&server, client_id),
        None,
        "nothing of it is on screen"
    );
    assert!(
        server
            .build_snapshot(client_id)
            .expect("snapshot")
            .pane_snapshots[0]
            .has_selection,
        "the client still holds a highlight in the pane"
    );
}

#[test]
fn a_highlight_starting_past_the_last_column_draws_no_row() {
    // The pane is 80 columns wide, so columns 90..=95 are off the right edge
    // and the row they name carries no span at all.
    let (mut server, pane_id, client_id) = build_runtime_with_terminal_input(b"hello");
    server
        .get_client_mut(client_id)
        .expect("client")
        .set_selection(
            pane_id,
            build_character_selection(
                GridPosition {
                    row_index: 0,
                    column_index: 90,
                },
                GridPosition {
                    row_index: 0,
                    column_index: 95,
                },
            ),
        );

    assert_eq!(get_selection_row_spans(&server, client_id), None);
}

#[test]
fn a_highlight_the_view_has_scrolled_past_is_not_drawn() {
    // 30 lines through a 24-row screen: rows 0..=6 are in history, and the view
    // follows live output, so a highlight back at row 1 is off screen.
    let mut terminal_input_bytes = Vec::new();
    for line_number in 0..30 {
        terminal_input_bytes.extend_from_slice(format!("line{line_number}\r\n").as_bytes());
    }
    let (mut server, pane_id, client_id) = build_runtime_with_terminal_input(&terminal_input_bytes);
    server
        .get_client_mut(client_id)
        .expect("client")
        .set_selection(
            pane_id,
            build_character_selection(
                GridPosition {
                    row_index: 1,
                    column_index: 0,
                },
                GridPosition {
                    row_index: 1,
                    column_index: 3,
                },
            ),
        );

    assert_eq!(
        get_selection_row_spans(&server, client_id),
        None,
        "nothing of it is on screen"
    );
    assert!(
        server
            .build_snapshot(client_id)
            .expect("snapshot")
            .pane_snapshots[0]
            .has_selection,
        "the client still holds a highlight in the pane, so the wheel still \
         scrolls koshi's own view"
    );
}

#[test]
fn scrolling_back_to_a_highlight_draws_it_again() {
    let mut terminal_input_bytes = Vec::new();
    for line_number in 0..30 {
        terminal_input_bytes.extend_from_slice(format!("line{line_number}\r\n").as_bytes());
    }
    let (mut server, pane_id, client_id) = build_runtime_with_terminal_input(&terminal_input_bytes);
    let mutable_client = server.get_client_mut(client_id).expect("client");
    mutable_client.set_selection(
        pane_id,
        build_character_selection(
            GridPosition {
                row_index: 1,
                column_index: 0,
            },
            GridPosition {
                row_index: 1,
                column_index: 3,
            },
        ),
    );
    // Scroll up far enough that line 1 is back on screen.
    mutable_client.set_scroll_offset(pane_id, 7);

    assert_eq!(
        get_selection_row_spans(&server, client_id),
        Some(vec![(1, 0, 3)]),
        "the same absolute row, now drawn at a screen row the scroll put it on"
    );
}

#[test]
fn a_highlight_running_off_the_top_of_the_view_starts_at_the_first_visible_row() {
    let mut terminal_input_bytes = Vec::new();
    for line_number in 0..30 {
        terminal_input_bytes.extend_from_slice(format!("line{line_number}\r\n").as_bytes());
    }
    let (mut server, pane_id, client_id) = build_runtime_with_terminal_input(&terminal_input_bytes);
    // Rows 0..=6 are in history and the view follows live, so the visible rows
    // are 7..=30. A highlight from row 2 to row 9 is half off the top.
    server
        .get_client_mut(client_id)
        .expect("client")
        .set_selection(
            pane_id,
            build_character_selection(
                GridPosition {
                    row_index: 2,
                    column_index: 4,
                },
                GridPosition {
                    row_index: 9,
                    column_index: 5,
                },
            ),
        );

    let visible_row_spans =
        get_selection_row_spans(&server, client_id).expect("the visible part is drawn");
    assert_eq!(
        visible_row_spans.first().copied(),
        Some((0, 0, 79)),
        "the first visible row starts at column 0, not the selection's own \
         start column, which is above the view"
    );
    assert_eq!(
        visible_row_spans.last().copied(),
        Some((2, 0, 5)),
        "and ends where it ends"
    );
}

// ============================================================================
// Pane titles: the reported working directory, the OSC title, and the screen
// that decides between them
// ============================================================================

/// The title the frame carries for the client's only pane.
fn get_pane_title(server: &Server, client_id: ClientId) -> Option<String> {
    server
        .build_snapshot(client_id)
        .expect("snapshot")
        .pane_snapshots[0]
        .pane_title
        .clone()
}

#[test]
fn the_pane_title_is_the_shells_reported_directory_on_the_primary_screen() {
    // OSC 2 names the window, OSC 7 reports the working directory. On the
    // primary screen the directory wins.
    let (server, _pane_id, client_id) = build_runtime_with_terminal_input(
        b"\x1b]2;window title\x07\x1b]7;file://localhost/tmp\x07",
    );

    assert_eq!(get_pane_title(&server, client_id), Some("/tmp".to_string()));
}

#[test]
fn the_pane_title_falls_back_to_the_osc_title_when_no_directory_was_reported() {
    let (server, _pane_id, client_id) =
        build_runtime_with_terminal_input(b"\x1b]2;window title\x07");

    assert_eq!(
        get_pane_title(&server, client_id),
        Some("window title".to_string())
    );
}

#[test]
fn the_pane_title_on_the_alternate_screen_is_the_apps_osc_title() {
    // `CSI ?1049h` enters the alternate screen; the reported directory no
    // longer names the pane there.
    let (server, _pane_id, client_id) = build_runtime_with_terminal_input(
        b"\x1b]2;window title\x07\x1b]7;file://localhost/tmp\x07\x1b[?1049h",
    );

    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert!(render_snapshot.pane_snapshots[0].is_on_alternate_screen);
    assert_eq!(
        render_snapshot.pane_snapshots[0].pane_title.as_deref(),
        Some("window title")
    );
}

#[test]
fn a_pane_on_the_alternate_screen_with_no_osc_title_has_none() {
    let (server, _pane_id, client_id) =
        build_runtime_with_terminal_input(b"\x1b]7;file://localhost/tmp\x07\x1b[?1049h");

    assert_eq!(get_pane_title(&server, client_id), None);
}

#[test]
fn format_display_path_is_bounded_and_filtered() {
    use super::format_display_path;

    let long_path = std::path::PathBuf::from(format!("/{}", "a".repeat(4_000)));
    assert!(
        format_display_path(&long_path).len() <= koshi_core::text::MAX_REPORTED_TEXT_BYTE_COUNT
    );

    let hostile_path = std::path::PathBuf::from("/tmp/a\u{7f}b\u{202e}c");
    assert_eq!(format_display_path(&hostile_path), "/tmp/abc");
}
