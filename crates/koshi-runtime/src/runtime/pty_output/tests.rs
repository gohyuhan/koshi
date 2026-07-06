//! Tests for PTY output handling: bytes reach only the owning pane's engine,
//! a decode carries across chunks, output schedules a render, device-query
//! replies are written back to the pane's PTY, and bytes for a pane with no
//! engine are dropped.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Instant;

use koshi_core::process::{PtySize, ShellKind, SpawnSpec};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_pty::backend::state::PtyBackend;
use koshi_terminal::engine::TerminalEngine;
use koshi_terminal::style::{Color, Style};
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;

use super::*;

/// A bare runtime with stub services and no sessions, plus the fake PTY
/// backend for asserting on writes. The sender is returned so the inbox stays
/// open.
fn new_runtime() -> (Runtime, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
    let fake = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake.clone();
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let runtime = Runtime::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx.clone(),
        TerminalCleanupGuard::new(),
    );
    (runtime, fake, tx)
}

/// Install an 8x3 terminal engine for a fresh pane id and return the id.
fn add_engine(rt: &mut Runtime) -> PaneId {
    let pane_id = PaneId::new();
    rt.terminal_engines
        .insert(pane_id, TerminalEngine::new(PtySize { cols: 8, rows: 3 }));
    pane_id
}

/// Register `pane_id` with the fake backend so its writes are recorded.
fn spawn_in_fake(fake: &FakePtyBackend, pane_id: PaneId) {
    let spec = SpawnSpec {
        program: PathBuf::from("/bin/zsh"),
        args: Vec::new(),
        cwd: None,
        env: BTreeMap::new(),
        shell_kind: ShellKind::Zsh,
    };
    fake.spawn(pane_id, spec, PtySize { cols: 8, rows: 3 })
        .expect("fake spawn succeeds");
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
    let (mut rt, _fake, _tx) = new_runtime();
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
    let (mut rt, _fake, _tx) = new_runtime();
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
    let (mut rt, _fake, _tx) = new_runtime();
    let pane = add_engine(&mut rt);

    rt.handle_pty_output(pane, b"hi");

    // PtyOutput was marked pending and nothing has rendered yet, so a render
    // is due immediately.
    assert!(rt.render_scheduler.poll(Instant::now()));
}

#[test]
fn a_device_querys_reply_is_written_back_to_the_pty() {
    let (mut rt, fake, _tx) = new_runtime();
    let pane = add_engine(&mut rt);
    spawn_in_fake(&fake, pane);

    // DSR 5 (operating status) embedded in ordinary output.
    rt.handle_pty_output(pane, b"hi\x1b[5n");

    assert_eq!(fake.writes(pane).unwrap(), vec![b"\x1b[0n".to_vec()]);
}

#[test]
fn replies_from_one_chunk_are_written_as_one_batch_in_query_order() {
    let (mut rt, fake, _tx) = new_runtime();
    let pane = add_engine(&mut rt);
    spawn_in_fake(&fake, pane);

    rt.handle_pty_output(pane, b"\x1b[5n\x1b[6n");

    assert_eq!(
        fake.writes(pane).unwrap(),
        vec![b"\x1b[0n\x1b[1;1R".to_vec()]
    );
}

#[test]
fn output_without_a_query_writes_nothing_back() {
    let (mut rt, fake, _tx) = new_runtime();
    let pane = add_engine(&mut rt);
    spawn_in_fake(&fake, pane);

    rt.handle_pty_output(pane, b"hi\x1b[31m");

    assert_eq!(fake.writes(pane).unwrap(), Vec::<Vec<u8>>::new());
}

#[test]
fn a_failed_reply_write_is_dropped_and_output_still_lands() {
    let (mut rt, fake, _tx) = new_runtime();
    // The engine exists but the pane was never spawned in the backend, so the
    // reply write fails with an unknown-pane error.
    let pane = add_engine(&mut rt);

    rt.handle_pty_output(pane, b"x\x1b[5n");

    // The chunk still reached the grid and scheduled a render; the failed
    // write left no record.
    assert_eq!(ch(&rt, pane, 0, 0), 'x');
    assert!(rt.render_scheduler.poll(Instant::now()));
    assert!(fake.writes(pane).is_err());
}

#[test]
fn bytes_for_a_pane_with_no_engine_are_ignored() {
    let (mut rt, _fake, _tx) = new_runtime();
    let live = add_engine(&mut rt);
    let gone = PaneId::new();

    rt.handle_pty_output(gone, b"\x1b[31mboom");

    // No engine changed and no render was scheduled.
    assert_eq!(ch(&rt, live, 0, 0), ' ');
    assert!(!rt.render_scheduler.poll(Instant::now()));
}
