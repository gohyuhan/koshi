//! Tests for the loop-facing driver surface: render-wakeup timing and
//! poll delegation to the scheduler, the live-pane check, and the abrupt
//! group-kill the panic path takes.

use std::collections::BTreeMap;
use std::sync::mpsc;

use koshi_core::geometry::Direction;
use koshi_core::ids::PaneId;
use koshi_core::process::{PtySize, SpawnSpec};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;
use crate::runtime::render_schedule::InvalidationReason;

use super::*;

const PANE_SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// A runtime sharing one fake backend, returned alongside it so a test can
/// assert on the kills the driver issues. The sender keeps the inbox open.
fn new_runtime_with_fake() -> (Runtime, Arc<FakePtyBackend>, mpsc::Sender<RuntimeEvent>) {
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
        Direction::Right,
    );
    (runtime, fake, tx)
}

/// Spawn a pane in the fake backend and park its handle in the runtime, so the
/// pane is live in both — the backend can record kills and the runtime counts
/// it as active.
fn spawn_and_park(rt: &mut Runtime, fake: &FakePtyBackend, pane: PaneId) {
    let handle = fake
        .spawn(
            pane,
            SpawnSpec::default_shell(None, BTreeMap::new()),
            PANE_SIZE,
        )
        .expect("spawn");
    rt.park_pane_pty(pane, handle, PANE_SIZE);
}

#[test]
fn no_panes_are_active_before_any_pane_is_parked() {
    let (rt, _fake, _tx) = new_runtime_with_fake();

    assert!(!rt.has_active_panes());
}

#[test]
fn a_parked_pane_makes_the_runtime_report_active() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();

    spawn_and_park(&mut rt, &fake, pane);

    assert!(rt.has_active_panes());
}

#[test]
fn the_panic_teardown_group_kills_every_pane_as_a_tree() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let first = PaneId::new();
    let second = PaneId::new();
    spawn_and_park(&mut rt, &fake, first);
    spawn_and_park(&mut rt, &fake, second);

    rt.kill_all_panes();

    assert_eq!(
        fake.kills(first).expect("first pane"),
        vec![KillPolicy::Tree]
    );
    assert_eq!(
        fake.kills(second).expect("second pane"),
        vec![KillPolicy::Tree]
    );
}

#[test]
fn nothing_is_pending_so_the_loop_sleeps_and_no_render_is_due() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    let now = Instant::now();

    assert_eq!(rt.next_render_wakeup(now), None);
    assert!(!rt.poll_render(now));
}

#[test]
fn a_pending_invalidation_is_due_at_once_then_clears_after_one_render() {
    let (mut rt, _fake, _tx) = new_runtime_with_fake();
    rt.render_scheduler
        .invalidate(InvalidationReason::PtyOutput);
    let now = Instant::now();

    assert_eq!(rt.next_render_wakeup(now), Some(Duration::ZERO));
    assert!(rt.poll_render(now));
    assert!(!rt.poll_render(now));
    assert_eq!(rt.next_render_wakeup(now), None);
}
