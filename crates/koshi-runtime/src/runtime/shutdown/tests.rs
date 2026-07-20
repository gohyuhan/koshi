//! Tests for the staged quit teardown: draining is entered, an explicit quit
//! group-kills immediately, a natural ending group-kills gracefully, and only
//! parked panes are killed.

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

use super::*;

const PANE_SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// A runtime sharing one fake backend, returned alongside it so a test can
/// assert on the kills shutdown issues. The sender keeps the inbox open.
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
/// pane is live in both — the backend can record kills and shutdown reaches it.
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
fn explicit_quit_group_kills_every_pane_immediately_as_a_tree() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();
    spawn_and_park(&mut rt, &fake, pane);
    rt.immediate_shutdown = true;

    rt.shutdown();

    assert!(rt.is_draining());
    assert_eq!(fake.kills(pane).expect("pane"), vec![KillPolicy::Tree]);
}

#[test]
fn a_natural_ending_group_kills_every_pane_gracefully_with_the_configured_timeout() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let first = PaneId::new();
    let second = PaneId::new();
    spawn_and_park(&mut rt, &fake, first);
    spawn_and_park(&mut rt, &fake, second);

    rt.shutdown();

    assert!(rt.is_draining());
    let graceful = KillPolicy::GracefulTree {
        timeout: GRACEFUL_TIMEOUT_DURATION,
    };
    assert_eq!(fake.kills(first).expect("first pane"), vec![graceful]);
    assert_eq!(fake.kills(second).expect("second pane"), vec![graceful]);
}

#[test]
fn shutdown_with_no_parked_panes_enters_draining_and_kills_nothing() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    // Spawn a pane in the backend but never park it, so it is not a live pane
    // the runtime tracks; shutdown must not reach it.
    let unparked = PaneId::new();
    fake.spawn(
        unparked,
        SpawnSpec::default_shell(None, BTreeMap::new()),
        PANE_SIZE,
    )
    .expect("spawn");

    rt.shutdown();

    assert!(rt.is_draining());
    assert_eq!(fake.kills(unparked).expect("pane"), Vec::new());
}

#[test]
fn calling_shutdown_again_requests_another_group_kill() {
    let (mut rt, fake, _tx) = new_runtime_with_fake();
    let pane = PaneId::new();
    spawn_and_park(&mut rt, &fake, pane);
    rt.immediate_shutdown = true;

    rt.shutdown();
    rt.shutdown();

    assert!(rt.is_draining());
    assert_eq!(
        fake.kills(pane).expect("pane"),
        vec![KillPolicy::Tree, KillPolicy::Tree]
    );
}
