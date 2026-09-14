//! Server-level integration: the pieces the event loop wires together across
//! the runtime submodules. A spawned pane's output reaches the inbox, the pane
//! reports active, and the graceful quit teardown group-kills it.

use std::collections::BTreeMap;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::ids::PaneId;
use koshi_core::process::{KillPolicy, PtySize, SpawnSpec};
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

const TEST_PANE_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};
const SERVER_TEST_DEADLINE_DURATION: Duration = Duration::from_secs(5);

#[test]
fn a_spawned_pane_forwards_output_reports_active_and_is_killed_on_graceful_shutdown() {
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let (event_sender, inbox_rx) = mpsc::channel();
    let mut server = Server::from_runtime_parts(pty_backend, inbox_rx, event_sender);

    assert!(!server.has_active_panes(), "a fresh server parks no pane");
    assert!(!server.is_draining(), "a fresh server is serving");

    let pane_id = PaneId::new();
    let pty_handle = fake_pty_backend
        .spawn_pane(
            pane_id,
            SpawnSpec::default_shell(None, BTreeMap::new()),
            TEST_PANE_SIZE,
        )
        .expect("spawn");
    server.park_pane_pty(pane_id, pty_handle, TEST_PANE_SIZE);

    fake_pty_backend
        .push_output(pane_id, b"hi".to_vec())
        .expect("push");
    match server
        .inbox_rx()
        .recv_timeout(SERVER_TEST_DEADLINE_DURATION)
    {
        Ok(RuntimeEvent::PtyOutput {
            pane_id: reported_pane_id,
            output_bytes,
        }) => {
            assert_eq!(reported_pane_id, pane_id);
            assert_eq!(output_bytes, b"hi");
        }
        other => panic!("expected PtyOutput, got {other:?}"),
    }

    assert!(server.has_active_panes());

    server.shutdown();

    assert!(server.is_draining());
    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(pane_id)
            .expect("pane"),
        vec![KillPolicy::GracefulTree {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
}
