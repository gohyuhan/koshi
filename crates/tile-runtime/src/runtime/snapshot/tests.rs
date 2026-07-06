//! Tests for the render-snapshot builder: mapping live runtime state (session,
//! tabs, client, terminal grids) into a `RenderSnapshot`, the per-client
//! invariants the renderer relies on, and the engine-less and dead-pane paths.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::SystemTime;

use tile_core::geometry::{Point, Rect, Size};
use tile_core::ids::{ClientId, PaneId, SessionId, TabId};
use tile_core::process::PtySize;
use tile_observability::cleanup::TerminalCleanupGuard;
use tile_pane::pane::lifecycle::PaneLifecycleEvent;
use tile_pane::pane::state::PaneRecord;
use tile_pty::backend::state::PtyBackend;
use tile_renderer::snapshot::PluginUiSnapshot;
use tile_session::client::{Client, ClientRegistry};
use tile_session::session::state::{Session, Tab};
use tile_terminal::engine::TerminalEngine;
use tile_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;
use crate::runtime::state::Runtime;

fn new_runtime() -> Runtime {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel::<RuntimeEvent>();
    Runtime::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        TerminalCleanupGuard::new(),
    )
}

/// A session with one tab (single-pane layout), the pane registered, and one
/// client attached viewing that tab focused on the pane.
fn session_with_client(viewport: Size) -> (Session, SessionId, TabId, PaneId, ClientId) {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();

    let mut session = Session::new(session_id, "s".to_string(), ClientRegistry::new());
    session
        .panes
        .insert(PaneRecord::new(pane_id, SystemTime::now()))
        .expect("unique pane id");
    session
        .tabs
        .insert(tab_id, Tab::new(tab_id, "t".to_string(), 0, pane_id));

    let mut client = Client::new(client_id, session_id, SystemTime::now(), viewport, tab_id);
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);

    (session, session_id, tab_id, pane_id, client_id)
}

#[test]
fn build_snapshot_for_an_unknown_client_is_none() {
    let rt = new_runtime();
    assert_eq!(rt.build_snapshot(ClientId::new()), None);
}

#[test]
fn build_snapshot_maps_session_tab_and_client() {
    let mut rt = new_runtime();
    let (session, session_id, tab_id, pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);
    rt.terminal_engines
        .insert(pane_id, TerminalEngine::new(PtySize { cols: 80, rows: 24 }));

    let snap = rt.build_snapshot(client_id).expect("snapshot");

    // Session + client identity.
    assert_eq!(snap.session.id, session_id);
    assert_eq!(snap.session.name, "s");
    assert_eq!(snap.client.id, client_id);
    assert_eq!(snap.client.viewport, Size { cols: 80, rows: 24 });
    assert_eq!(snap.client.focused_pane, Some(pane_id));

    // The load-bearing per-client invariant the renderer asserts.
    assert_eq!(snap.client.active_tab, tab_id);
    assert_eq!(snap.session.active_tab.id, tab_id);

    // The solved tab: one visible pane slot.
    assert_eq!(
        snap.session.active_tab.effective_size,
        Size { cols: 80, rows: 24 }
    );
    assert_eq!(snap.session.active_tab.layout_solved.len(), 1);
    let slot = &snap.session.active_tab.layout_solved[0];
    assert_eq!(slot.pane_id, pane_id);
    assert!(slot.visible);
    // Single pane over 80×24: outer = the whole tab, inner = inset by the 1-cell border.
    assert_eq!(
        slot.rect,
        Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 24 })
    );
    assert_eq!(
        slot.inner_rect,
        Some(Rect::new(Point { x: 1, y: 1 }, Size { cols: 78, rows: 22 }))
    );
    assert!(!slot.suppressed);
    assert!(!slot.dead);

    // Tab metadata: a single active tab at index 0.
    assert_eq!(snap.session.tabs_metadata.len(), 1);
    assert_eq!(snap.session.tabs_metadata[0].id, tab_id);
    assert_eq!(snap.session.tabs_metadata[0].index, 0);
    assert!(snap.session.tabs_metadata[0].active);

    // One pane content entry, with a grid view (engine present).
    assert_eq!(snap.panes.len(), 1);
    assert_eq!(snap.panes[0].id, pane_id);
    let grid_view = snap.panes[0].grid_view.as_ref().expect("grid view");
    assert_eq!(grid_view.view_offset, 0);
    assert_eq!(grid_view.grid.dimensions(), (24, 80));

    // No plugin UI for a stock session.
    assert_eq!(snap.plugin_ui, PluginUiSnapshot::default());
}

#[test]
fn a_pane_without_a_terminal_engine_has_no_grid_view() {
    let mut rt = new_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);
    // No engine inserted for pane_id.

    let snap = rt.build_snapshot(client_id).expect("snapshot");
    assert_eq!(snap.panes.len(), 1);
    assert_eq!(snap.panes[0].id, pane_id);
    assert_eq!(snap.panes[0].grid_view, None);
    assert!(!snap.panes[0].cursor.visible);
    assert_eq!(snap.panes[0].title, None);
}

#[test]
fn build_snapshot_carries_the_live_terminal_grid_and_cursor() {
    let mut rt = new_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);
    let mut engine = TerminalEngine::new(PtySize { cols: 80, rows: 24 });
    let _ = engine.advance(b"hi");
    rt.terminal_engines.insert(pane_id, engine);

    let snap = rt.build_snapshot(client_id).expect("snapshot");
    let pane = &snap.panes[0];

    // Cursor advanced two columns, still visible.
    assert_eq!(pane.cursor.row, 0);
    assert_eq!(pane.cursor.col, 2);
    assert!(pane.cursor.visible);

    // The shared grid handle carries the printed cells at offset 0.
    let grid_view = pane.grid_view.as_ref().expect("grid view");
    assert_eq!(grid_view.view_offset, 0);
    assert_eq!(grid_view.grid.cell(0, 0).map(|c| c.ch()), Some('h'));
    assert_eq!(grid_view.grid.cell(0, 1).map(|c| c.ch()), Some('i'));

    // Mode/scrollback passthroughs read from the engine.
    assert!(!pane.reverse_video);
    assert_eq!(pane.scrollback.retained_lines, 0);
    assert!(!pane.scrollback.truncated);
}

#[test]
fn a_frozen_snapshot_keeps_its_grid_when_the_engine_writes_again() {
    let mut rt = new_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);
    let mut engine = TerminalEngine::new(PtySize { cols: 80, rows: 24 });
    let _ = engine.advance(b"A");
    rt.terminal_engines.insert(pane_id, engine);

    // Freeze frame 1 while cell (0, 0) holds 'A'.
    let frame1 = rt.build_snapshot(client_id).expect("snapshot");

    // The engine writes more output after the freeze: CR home, then overwrite (0, 0).
    let _ = rt
        .terminal_engines
        .get_mut(&pane_id)
        .expect("engine")
        .advance(b"\rB");

    // Copy-on-write: frame 1's shared grid still shows the pre-write glyph — the
    // later `active_grid_mut` cloned the buffer instead of mutating the frozen one.
    let grid1 = &frame1.panes[0].grid_view.as_ref().expect("grid view").grid;
    assert_eq!(grid1.cell(0, 0).map(|c| c.ch()), Some('A'));

    // A fresh snapshot reflects the new write.
    let frame2 = rt.build_snapshot(client_id).expect("snapshot");
    let grid2 = &frame2.panes[0].grid_view.as_ref().expect("grid view").grid;
    assert_eq!(grid2.cell(0, 0).map(|c| c.ch()), Some('B'));
}

#[test]
fn effective_size_is_the_min_viewport_across_clients_not_the_requesters() {
    let mut rt = new_runtime();
    let (mut session, session_id, tab_id, pane_id, big_client) =
        session_with_client(Size { cols: 80, rows: 24 });

    // A second client views the same tab at a smaller viewport.
    let small_client = ClientId::new();
    let mut client = Client::new(
        small_client,
        session_id,
        SystemTime::now(),
        Size { cols: 40, rows: 10 },
        tab_id,
    );
    client.update_focused_pane(tab_id, pane_id);
    session.attach_client(client);
    rt.sessions.insert(session_id, session);

    let snap = rt.build_snapshot(big_client).expect("snapshot");
    // The requesting client's own viewport is unchanged...
    assert_eq!(snap.client.viewport, Size { cols: 80, rows: 24 });
    // ...but the tab is solved at the shared minimum, which the renderer letterboxes.
    assert_eq!(
        snap.session.active_tab.effective_size,
        Size { cols: 40, rows: 10 }
    );
}

#[test]
fn an_exited_pane_is_marked_dead_but_stays_visible() {
    let mut rt = new_runtime();
    let (mut session, session_id, _tab_id, pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    {
        let record = session.panes.get_mut(pane_id).expect("record");
        let _ = record.update_lifecycle(PaneLifecycleEvent::ProcessStarted);
        let _ = record.update_lifecycle(PaneLifecycleEvent::ProcessExited {
            code: Some(0),
            at: SystemTime::now(),
        });
    }
    rt.sessions.insert(session_id, session);

    let snap = rt.build_snapshot(client_id).expect("snapshot");
    let slot = &snap.session.active_tab.layout_solved[0];
    assert!(slot.dead);
    // `dead` is orthogonal to visibility: an exited pane stays laid out.
    assert!(slot.visible);
}

#[test]
fn tabs_metadata_covers_every_tab_in_index_order_with_the_viewed_tab_active() {
    let mut rt = new_runtime();
    let (mut session, session_id, tab0, _pane0, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });

    // A second tab the client is not viewing.
    let tab1 = TabId::new();
    let pane1 = PaneId::new();
    session
        .panes
        .insert(PaneRecord::new(pane1, SystemTime::now()))
        .expect("unique pane id");
    session
        .tabs
        .insert(tab1, Tab::new(tab1, "t2".to_string(), 1, pane1));
    rt.sessions.insert(session_id, session);

    let snap = rt.build_snapshot(client_id).expect("snapshot");
    assert_eq!(snap.session.tabs_metadata.len(), 2);
    assert_eq!(snap.session.tabs_metadata[0].index, 0);
    assert_eq!(snap.session.tabs_metadata[1].index, 1);

    // Only the client's viewed tab is active.
    let active: Vec<TabId> = snap
        .session
        .tabs_metadata
        .iter()
        .filter(|meta| meta.active)
        .map(|meta| meta.id)
        .collect();
    assert_eq!(active, vec![tab0]);
    assert_eq!(snap.session.active_tab.id, tab0);
}

#[test]
fn snapshot_follows_live_output_when_the_client_has_not_scrolled() {
    let mut rt = new_runtime();
    let (session, session_id, _tab, pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);
    rt.terminal_engines
        .insert(pane_id, TerminalEngine::new(PtySize { cols: 8, rows: 1 }));
    rt.handle_pty_output(pane_id, b"\n\n\n"); // three retained lines

    let snap = rt.build_snapshot(client_id).expect("snapshot");
    let pane = snap.panes.iter().find(|p| p.id == pane_id).expect("pane");
    assert_eq!(pane.grid_view.as_ref().unwrap().view_offset, 0);
    assert_eq!(pane.scrollback.retained_lines, 3);
}

#[test]
fn snapshot_carries_the_clients_scrolled_back_offset() {
    let mut rt = new_runtime();
    let (session, session_id, _tab, pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);
    rt.terminal_engines
        .insert(pane_id, TerminalEngine::new(PtySize { cols: 8, rows: 1 }));
    rt.handle_pty_output(pane_id, b"\n\n\n");
    rt.scroll_up(client_id, pane_id, 2);

    let snap = rt.build_snapshot(client_id).expect("snapshot");
    let pane = snap.panes.iter().find(|p| p.id == pane_id).expect("pane");
    // The scrolled offset reaches the renderer as the view offset.
    assert_eq!(pane.grid_view.as_ref().unwrap().view_offset, 2);
    assert_eq!(pane.scrollback.retained_lines, 3);
}

#[test]
fn snapshot_reports_a_live_offset_for_a_scrolled_client_on_the_alternate_screen() {
    let mut rt = new_runtime();
    let (session, session_id, _tab, pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);
    rt.terminal_engines
        .insert(pane_id, TerminalEngine::new(PtySize { cols: 8, rows: 1 }));
    rt.handle_pty_output(pane_id, b"\n\n\n");
    rt.scroll_up(client_id, pane_id, 2);
    rt.handle_pty_output(pane_id, b"\x1b[?1049h"); // enter the alternate screen

    let snap = rt.build_snapshot(client_id).expect("snapshot");
    let pane = snap.panes.iter().find(|p| p.id == pane_id).expect("pane");
    // The alternate screen keeps no scrollback: the parked offset does not apply,
    // so the view follows live and the renderer sees offset 0.
    assert_eq!(pane.grid_view.as_ref().unwrap().view_offset, 0);
}
