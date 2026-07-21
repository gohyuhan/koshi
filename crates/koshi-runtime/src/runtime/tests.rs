//! Server-level integration: the pieces the event loop wires together across
//! the runtime submodules. A spawned pane's output reaches the inbox, the pane
//! reports active, and the graceful quit teardown group-kills it.

use std::collections::BTreeMap;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::geometry::Direction;
use koshi_core::ids::PaneId;
use koshi_core::process::{KillPolicy, PtySize, SpawnSpec};
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage, SnapshotProvider, Storage};
use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

const PANE_SIZE: PtySize = PtySize { cols: 80, rows: 24 };
const DEADLINE: Duration = Duration::from_secs(5);

#[test]
fn a_spawned_pane_forwards_output_reports_active_and_is_killed_on_graceful_shutdown() {
    let fake = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake.clone();
    let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
    let storage: Arc<dyn Storage> = Arc::new(NullStorage);
    let (tx, inbox_rx) = mpsc::channel();
    let mut rt = Server::new(
        pty_backend,
        snapshot_provider,
        storage,
        inbox_rx,
        tx,
        Direction::Right,
    );

    let pane = PaneId::new();
    let handle = fake
        .spawn(
            pane,
            SpawnSpec::default_shell(None, BTreeMap::new()),
            PANE_SIZE,
        )
        .expect("spawn");
    rt.park_pane_pty(pane, handle, PANE_SIZE);

    fake.push_output(pane, b"hi".to_vec()).expect("push");
    assert_eq!(
        rt.inbox_rx().recv_timeout(DEADLINE),
        Ok(RuntimeEvent::PtyOutput {
            pane_id: pane,
            bytes: b"hi".to_vec(),
        })
    );

    assert!(rt.has_active_panes());

    rt.shutdown();

    assert!(rt.is_draining());
    assert_eq!(
        fake.kills(pane).expect("pane"),
        vec![KillPolicy::GracefulTree {
            timeout: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
}
