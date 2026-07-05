//! Tests for PTY output handling: bytes reach only the owning pane's engine,
//! a decode carries across chunks, output schedules a render, and bytes for a
//! pane with no engine are dropped.

use std::sync::{mpsc, Arc};
use std::time::Instant;

use tile_core::process::PtySize;
use tile_observability::cleanup::TerminalCleanupGuard;
use tile_pty::backend::state::PtyBackend;
use tile_terminal::engine::TerminalEngine;
use tile_terminal::style::{Color, Style};
use tile_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;

use super::*;

struct DummySnapshotProvider;
impl SnapshotProvider for DummySnapshotProvider {}

struct DummyStorage;
impl Storage for DummyStorage {}

/// A bare runtime with stub services and no sessions. The sender is returned
/// so the inbox stays open.
fn new_runtime() -> (Runtime, mpsc::Sender<RuntimeEvent>) {
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::new());
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(DummySnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(DummyStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Runtime::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        TerminalCleanupGuard::new(),
    );
    (runtime, tx)
}

/// Install an 8x3 terminal engine for a fresh pane id and return the id.
fn add_engine(rt: &mut Runtime) -> PaneId {
    let pane_id = PaneId::new();
    rt.terminal_engines
        .insert(pane_id, TerminalEngine::new(PtySize { cols: 8, rows: 3 }));
    pane_id
}

/// The character at (`row`, `col`) on `pane_id`'s active grid.
fn ch(rt: &Runtime, pane_id: PaneId, row: u16, col: u16) -> char {
    rt.terminal_engines()[&pane_id]
        .state()
        .active_grid()
        .cell(row, col)
        .expect("cell in bounds")
        .ch()
}

#[test]
fn bytes_update_only_the_owning_panes_grid() {
    let (mut rt, _tx) = new_runtime();
    let pane = add_engine(&mut rt);
    let other = add_engine(&mut rt);

    rt.handle_pty_output(pane, b"hi");

    assert_eq!(ch(&rt, pane, 0, 0), 'h');
    assert_eq!(ch(&rt, pane, 0, 1), 'i');
    assert_eq!(
        rt.terminal_engines()[&pane]
            .state()
            .active_cursor_position(),
        (0, 2)
    );

    // The other pane's engine is untouched.
    assert_eq!(ch(&rt, other, 0, 0), ' ');
    assert_eq!(
        rt.terminal_engines()[&other]
            .state()
            .active_cursor_position(),
        (0, 0)
    );
}

#[test]
fn an_escape_sequence_split_across_two_events_decodes_once() {
    let (mut rt, _tx) = new_runtime();
    let pane = add_engine(&mut rt);

    // SGR 31 (red foreground) split mid-sequence across two output events:
    // the pane's parser carries the partial sequence between handler calls.
    rt.handle_pty_output(pane, b"\x1b[3");
    rt.handle_pty_output(pane, b"1mx");

    let engines = rt.terminal_engines();
    let cell = engines[&pane]
        .state()
        .active_grid()
        .cell(0, 0)
        .expect("cell in bounds");
    let mut red = Style::default();
    red.set_fg(Color::Indexed(1));
    assert_eq!(cell.ch(), 'x');
    assert_eq!(cell.style(), red);
}

#[test]
fn output_schedules_a_render() {
    let (mut rt, _tx) = new_runtime();
    let pane = add_engine(&mut rt);

    rt.handle_pty_output(pane, b"hi");

    // PtyOutput was marked pending and nothing has rendered yet, so a render
    // is due immediately.
    assert!(rt.render_scheduler.poll(Instant::now()));
}

#[test]
fn bytes_for_a_pane_with_no_engine_are_ignored() {
    let (mut rt, _tx) = new_runtime();
    let live = add_engine(&mut rt);
    let gone = PaneId::new();

    rt.handle_pty_output(gone, b"\x1b[31mboom");

    // No engine changed and no render was scheduled.
    assert_eq!(ch(&rt, live, 0, 0), ' ');
    assert!(!rt.render_scheduler.poll(Instant::now()));
}
