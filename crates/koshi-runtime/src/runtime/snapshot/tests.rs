//! Tests for the render-snapshot builder: mapping live runtime state (session,
//! tabs, client, terminal grids) into a `RenderSnapshot`, the per-client
//! invariants the renderer relies on, and the engine-less and dead-pane paths.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::SystemTime;

use koshi_config::types::{RgbColor, ThemeConfig};
use koshi_core::command::{GridPos, Selection, SelectionKind};
use koshi_core::geometry::{Direction, Point, Rect, Size};
use koshi_core::ids::{ClientId, PaneId, SessionId, TabId};
use koshi_core::lock::LockMode;
use koshi_core::process::PtySize;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_pane::pane::lifecycle::PaneLifecycleEvent;
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_renderer::snapshot::PluginUiSnapshot;
use koshi_renderer::theme::Theme;
use koshi_session::client::{Client, ClientRegistry};
use koshi_session::session::state::{Session, Tab};
use koshi_terminal::engine::TerminalEngine;
use koshi_terminal::state::CursorShape;
use koshi_test_support::fake_pty::FakePtyBackend;
use ratatui::style::Color;

use super::resolve_theme;
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
        Direction::Right,
    )
}

/// A session with one tab (single-pane layout), the pane registered, and one
/// client attached viewing that tab focused on the pane.
fn session_with_client(viewport: Size) -> (Session, SessionId, TabId, PaneId, ClientId) {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let client_id = ClientId::new();

    let mut session = Session::new(
        session_id,
        "s".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    );
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
        Size { cols: 80, rows: 22 }
    );
    assert_eq!(snap.session.active_tab.layout_solved.len(), 1);
    let slot = &snap.session.active_tab.layout_solved[0];
    assert_eq!(slot.pane_id, pane_id);
    assert!(slot.visible);
    // Full 80×24 client leaves an 80×22 pane region; border insets content.
    assert_eq!(
        slot.rect,
        Rect::new(Point { x: 0, y: 0 }, Size { cols: 80, rows: 22 })
    );
    assert_eq!(
        slot.inner_rect,
        Some(Rect::new(Point { x: 1, y: 1 }, Size { cols: 78, rows: 20 }))
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

    // No sequence pends before a prefix key is pressed.
    assert_eq!(snap.client.pending_sequence, None);
}

#[test]
fn build_snapshot_carries_the_hints_for_the_clients_mode() {
    let mut rt = new_runtime();
    let (session, session_id, _tab_id, _pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);

    // Normal mode: the shipped normal-mode bindings surface as hint data.
    let snap = rt.build_snapshot(client_id).expect("snapshot");
    assert_eq!(snap.client.lock_mode, LockMode::Normal);
    assert_eq!(snap.keymap_hints.entries.len(), 22);
    assert!(!snap.keymap_hints.reverted);

    // Locked mode: the same frame path now carries only the pinned unlock.
    rt.sessions
        .get_mut(&session_id)
        .expect("session")
        .clients
        .get_mut(client_id)
        .expect("client")
        .update_lock_mode(LockMode::Locked);
    let snap = rt.build_snapshot(client_id).expect("snapshot");
    assert_eq!(snap.client.lock_mode, LockMode::Locked);
    // The reserved unlock (pinned) plus the quit and mouse-select chords.
    assert_eq!(snap.keymap_hints.entries.len(), 3);
    assert!(snap
        .keymap_hints
        .entries
        .iter()
        .any(|entry| entry.label == "Unlock" && entry.pinned));
    assert!(snap
        .keymap_hints
        .entries
        .iter()
        .any(|entry| entry.label == "Quit" && !entry.pinned));
}

#[test]
fn mouse_select_mode_flips_its_hint_label() {
    let mut rt = new_runtime();
    let (session, session_id, _tab_id, _pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);

    let has_label = |rt: &mut Runtime, label: &str| {
        rt.build_snapshot(client_id)
            .expect("snapshot")
            .keymap_hints
            .entries
            .iter()
            .any(|entry| entry.label == label)
    };

    // Off: the hint invites turning selection on.
    assert!(has_label(&mut rt, "Mouse Select"));
    assert!(!has_label(&mut rt, "Mouse Unselect"));

    // On: the same binding's hint flips to the off action, the way lock flips
    // to unlock.
    rt.sessions
        .get_mut(&session_id)
        .expect("session")
        .clients
        .get_mut(client_id)
        .expect("client")
        .toggle_mouse_select();
    assert!(has_label(&mut rt, "Mouse Unselect"));
    assert!(!has_label(&mut rt, "Mouse Select"));
}

#[test]
fn build_snapshot_carries_the_runtime_theme() {
    let mut rt = new_runtime();
    let (session, session_id, _tab_id, _pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);

    // A fresh runtime carries the stock theme.
    let snap = rt.build_snapshot(client_id).expect("snapshot");
    assert_eq!(snap.theme, Theme::default());

    // A replaced runtime theme reaches the next frame.
    let custom = Theme {
        ramp_start: (0xff, 0x00, 0x00),
        ..Theme::default()
    };
    rt.theme = custom;
    let snap = rt.build_snapshot(client_id).expect("snapshot");
    assert_eq!(snap.theme, custom);
}

/// Resolving the default config theme yields exactly the renderer's default
/// theme: the two crates' stock palettes never drift apart.
#[test]
fn resolving_the_default_config_theme_is_the_default_theme() {
    assert_eq!(resolve_theme(&ThemeConfig::default()), Theme::default());
}

/// Each palette role lands on its matching theme field as a truecolor.
#[test]
fn resolve_theme_maps_every_palette_role() {
    let mut config = ThemeConfig::default();
    config.colors.ramp_start = RgbColor::new(0x01, 0x02, 0x03);
    config.colors.ramp_end = RgbColor::new(0x04, 0x05, 0x06);
    config.colors.on_ramp = RgbColor::new(0x07, 0x08, 0x09);
    config.colors.on_ramp_dim = RgbColor::new(0x0a, 0x0b, 0x0c);
    config.colors.accent = RgbColor::new(0x0d, 0x0e, 0x0f);
    config.colors.on_accent = RgbColor::new(0x10, 0x11, 0x12);
    config.colors.border_focused = RgbColor::new(0x13, 0x14, 0x15);
    config.colors.border_unfocused = RgbColor::new(0x16, 0x17, 0x18);
    config.colors.border_hover = RgbColor::new(0x22, 0x23, 0x24);
    config.colors.stack_header_fg = RgbColor::new(0x19, 0x1a, 0x1b);
    config.colors.stack_header_bg = RgbColor::new(0x1c, 0x1d, 0x1e);
    config.colors.letterbox = RgbColor::new(0x1f, 0x20, 0x21);
    config.colors.bar_bg = RgbColor::new(0x25, 0x26, 0x27);

    let theme = resolve_theme(&config);
    assert_eq!(theme.ramp_start, (0x01, 0x02, 0x03));
    assert_eq!(theme.ramp_end, (0x04, 0x05, 0x06));
    assert_eq!(theme.on_ramp, Color::Rgb(0x07, 0x08, 0x09));
    assert_eq!(theme.on_ramp_dim, Color::Rgb(0x0a, 0x0b, 0x0c));
    assert_eq!(theme.accent, Color::Rgb(0x0d, 0x0e, 0x0f));
    assert_eq!(theme.on_accent, Color::Rgb(0x10, 0x11, 0x12));
    assert_eq!(theme.border_focused, Color::Rgb(0x13, 0x14, 0x15));
    assert_eq!(theme.border_unfocused, Color::Rgb(0x16, 0x17, 0x18));
    assert_eq!(theme.border_hover, Color::Rgb(0x22, 0x23, 0x24));
    assert_eq!(theme.stack_header_fg, Color::Rgb(0x19, 0x1a, 0x1b));
    assert_eq!(theme.stack_header_bg, Color::Rgb(0x1c, 0x1d, 0x1e));
    assert_eq!(theme.letterbox, Color::Rgb(0x1f, 0x20, 0x21));
    assert_eq!(theme.bar_bg, Color::Rgb(0x25, 0x26, 0x27));
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

    // A shell that never sent DECSCUSR has asked for no shape at all.
    assert_eq!(pane.cursor.shape, None);
    assert!(!pane.cursor.blink);
}

#[test]
fn build_snapshot_carries_the_cursor_style_the_pane_asked_for() {
    // The bytes vim writes on entering insert mode: DECSCUSR "blinking bar".
    // They must reach the snapshot, which is what lets the app style the outer
    // terminal's cursor to match — a block in normal mode, a bar in insert.
    let mut rt = new_runtime();
    let (session, session_id, _tab_id, pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);
    rt.terminal_engines
        .insert(pane_id, TerminalEngine::new(PtySize { cols: 80, rows: 24 }));

    rt.handle_pty_output(pane_id, b"\x1b[5 q");
    let snap = rt.build_snapshot(client_id).expect("snapshot");
    assert_eq!(snap.panes[0].cursor.shape, Some(CursorShape::Bar));
    assert!(snap.panes[0].cursor.blink);

    // Leaving insert mode: back to a steady block.
    rt.handle_pty_output(pane_id, b"\x1b[2 q");
    let snap = rt.build_snapshot(client_id).expect("snapshot");
    assert_eq!(snap.panes[0].cursor.shape, Some(CursorShape::Block));
    assert!(!snap.panes[0].cursor.blink);

    // vim exiting: `CSI 0 SP q` undoes its cursor, and the pane is back to
    // asking for nothing — the user's own terminal cursor stands again.
    rt.handle_pty_output(pane_id, b"\x1b[0 q");
    let snap = rt.build_snapshot(client_id).expect("snapshot");
    assert_eq!(snap.panes[0].cursor.shape, None);
    assert!(!snap.panes[0].cursor.blink);
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
        Size { cols: 40, rows: 8 }
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
    // The alternate screen keeps no scrollback: the client's stored offset does
    // not apply there, so the renderer sees effective offset 0.
    assert_eq!(pane.grid_view.as_ref().unwrap().view_offset, 0);
}

#[test]
fn shorten_home_replaces_the_prefix_only_on_a_path_boundary() {
    use super::shorten_home;
    use std::path::Path;
    let home = Some(Path::new("/Users/ab"));
    assert_eq!(shorten_home(Path::new("/Users/ab"), home), "~");
    assert_eq!(shorten_home(Path::new("/Users/ab/koshi"), home), "~/koshi");
    // A sibling directory sharing the prefix text is NOT under home.
    assert_eq!(
        shorten_home(Path::new("/Users/ab2/x"), home),
        "/Users/ab2/x"
    );
    assert_eq!(shorten_home(Path::new("/tmp"), None), "/tmp");
}

// ============================================================================
// Highlight resolution: absolute line numbers to the rows a frame shows
// ============================================================================

/// A runtime with one client and a pane whose terminal has run `bytes`.
fn runtime_with_text(bytes: &[u8]) -> (Runtime, PaneId, ClientId) {
    let mut rt = new_runtime();
    let (session, session_id, _tab, pane_id, client_id) =
        session_with_client(Size { cols: 80, rows: 24 });
    rt.sessions.insert(session_id, session);
    let mut engine = TerminalEngine::new(PtySize { cols: 80, rows: 24 });
    let _ = engine.advance(bytes);
    rt.terminal_engines.insert(pane_id, engine);
    (rt, pane_id, client_id)
}

/// The highlight rows the frame carries for the client's only pane.
fn spans(rt: &Runtime, client: ClientId) -> Option<Vec<(u16, u16, u16)>> {
    let snap = rt.build_snapshot(client).expect("snapshot");
    snap.panes[0]
        .selection
        .as_ref()
        .map(|spans| spans.rows.clone())
}

fn character(anchor: GridPos, cursor: GridPos) -> Selection {
    Selection {
        kind: SelectionKind::Character,
        anchor,
        cursor,
    }
}

#[test]
fn a_pane_with_no_highlight_carries_none() {
    let (rt, _pane, client) = runtime_with_text(b"hello");
    assert_eq!(spans(&rt, client), None);
}

#[test]
fn a_highlight_on_one_row_is_one_span() {
    let (mut rt, pane, client) = runtime_with_text(b"hello world");
    rt.client_mut(client).expect("client").set_selection(
        pane,
        character(GridPos { row: 0, col: 6 }, GridPos { row: 0, col: 10 }),
    );

    assert_eq!(spans(&rt, client), Some(vec![(0, 6, 10)]));
}

#[test]
fn a_highlight_over_three_rows_runs_with_the_text() {
    let (mut rt, pane, client) = runtime_with_text(b"a\r\nb\r\nc\r\nd");
    // From column 12 of row 1 to column 33 of row 3: the first row runs to its
    // end, the middle row is whole, the last stops at its column.
    rt.client_mut(client).expect("client").set_selection(
        pane,
        character(GridPos { row: 1, col: 12 }, GridPos { row: 3, col: 33 }),
    );

    assert_eq!(
        spans(&rt, client),
        Some(vec![(1, 12, 79), (2, 0, 79), (3, 0, 33)])
    );
}

#[test]
fn a_block_highlight_is_the_same_columns_on_every_row() {
    let (mut rt, pane, client) = runtime_with_text(b"a\r\nb\r\nc");
    rt.client_mut(client).expect("client").set_selection(
        pane,
        Selection {
            kind: SelectionKind::Block,
            anchor: GridPos { row: 0, col: 4 },
            cursor: GridPos { row: 2, col: 9 },
        },
    );

    assert_eq!(
        spans(&rt, client),
        Some(vec![(0, 4, 9), (1, 4, 9), (2, 4, 9)]),
        "a rectangle, not a run of text"
    );
}

#[test]
fn a_block_dragged_leftward_still_covers_the_columns_between() {
    let (mut rt, pane, client) = runtime_with_text(b"a\r\nb");
    // The anchor's column is to the RIGHT of the cursor's.
    rt.client_mut(client).expect("client").set_selection(
        pane,
        Selection {
            kind: SelectionKind::Block,
            anchor: GridPos { row: 0, col: 9 },
            cursor: GridPos { row: 1, col: 4 },
        },
    );

    assert_eq!(spans(&rt, client), Some(vec![(0, 4, 9), (1, 4, 9)]));
}

#[test]
fn a_highlight_ending_on_a_wide_glyph_covers_its_whole_cell() {
    // `a世b`: the wide glyph is at column 1 and its blank half at column 2. A
    // selection ending on the glyph reaches its left column, and the renderer
    // paints the 2-wide glyph from there while skipping the width-0 half — so
    // the highlight covers the whole glyph and can never land on half of one.
    let (mut rt, pane, client) = runtime_with_text("a世b".as_bytes());
    rt.client_mut(client).expect("client").set_selection(
        pane,
        character(GridPos { row: 0, col: 0 }, GridPos { row: 0, col: 1 }),
    );

    assert_eq!(spans(&rt, client), Some(vec![(0, 0, 1)]));
}

#[test]
fn a_highlight_the_view_has_scrolled_past_is_not_drawn() {
    // 30 lines through a 24-row screen: rows 0..=6 are in history, and the view
    // follows live output, so a highlight back at row 1 is off screen.
    let mut bytes = Vec::new();
    for i in 0..30 {
        bytes.extend_from_slice(format!("line{i}\r\n").as_bytes());
    }
    let (mut rt, pane, client) = runtime_with_text(&bytes);
    rt.client_mut(client).expect("client").set_selection(
        pane,
        character(GridPos { row: 1, col: 0 }, GridPos { row: 1, col: 3 }),
    );

    assert_eq!(spans(&rt, client), None, "nothing of it is on screen");
}

#[test]
fn scrolling_back_to_a_highlight_draws_it_again() {
    let mut bytes = Vec::new();
    for i in 0..30 {
        bytes.extend_from_slice(format!("line{i}\r\n").as_bytes());
    }
    let (mut rt, pane, client) = runtime_with_text(&bytes);
    let client_mut = rt.client_mut(client).expect("client");
    client_mut.set_selection(
        pane,
        character(GridPos { row: 1, col: 0 }, GridPos { row: 1, col: 3 }),
    );
    // Scroll up far enough that line 1 is back on screen.
    client_mut.set_scroll_offset(pane, 7);

    assert_eq!(
        spans(&rt, client),
        Some(vec![(1, 0, 3)]),
        "the same absolute row, now drawn at a screen row the scroll put it on"
    );
}

#[test]
fn a_highlight_running_off_the_top_of_the_view_starts_at_the_first_visible_row() {
    let mut bytes = Vec::new();
    for i in 0..30 {
        bytes.extend_from_slice(format!("line{i}\r\n").as_bytes());
    }
    let (mut rt, pane, client) = runtime_with_text(&bytes);
    // Rows 0..=6 are in history and the view follows live, so the visible rows
    // are 7..=30. A highlight from row 2 to row 9 is half off the top.
    rt.client_mut(client).expect("client").set_selection(
        pane,
        character(GridPos { row: 2, col: 4 }, GridPos { row: 9, col: 5 }),
    );

    let rows = spans(&rt, client).expect("the visible part is drawn");
    assert_eq!(
        rows.first().copied(),
        Some((0, 0, 79)),
        "the first visible row starts at column 0, not the selection's own \
         start column, which is above the view"
    );
    assert_eq!(
        rows.last().copied(),
        Some((2, 0, 5)),
        "and ends where it ends"
    );
}
