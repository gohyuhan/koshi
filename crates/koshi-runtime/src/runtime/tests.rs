//! Server-level integration: the pieces the event loop wires together across
//! the runtime submodules. A spawned pane's output reaches the inbox, the pane
//! reports active, and the graceful quit teardown group-kills it.

use std::collections::BTreeMap;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::runtime::pty_inbox::InboxSink;
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::ids::PaneId;
use koshi_core::process::{KillPolicy, PtySize, SpawnSpec};
use koshi_pty::backend::state::PtyBackend;
use koshi_test_support::fake_pty::FakePtyBackend;

use koshi_renderer::snapshot::{CommittedRegions, MouseFrame, RenderSnapshot};

use crate::runtime::event::RuntimeEvent;
use crate::server::Server;

/// The mouse frame for `render_snapshot` under the compiled-in region solve for
/// its client's viewport, at input revision `0`.
pub(crate) fn build_mouse_frame(render_snapshot: RenderSnapshot) -> MouseFrame {
    let committed_regions =
        CommittedRegions::build_core(render_snapshot.client_snapshot.viewport_size, 0);
    MouseFrame::from_snapshot(&render_snapshot, committed_regions)
}

const TEST_PANE_SIZE: PtySize = PtySize {
    column_count: 80,
    row_count: 24,
};
const SERVER_TEST_DEADLINE_DURATION: Duration = Duration::from_secs(5);

#[test]
fn a_spawned_pane_forwards_output_reports_active_and_is_killed_on_graceful_shutdown() {
    let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender),
    )));
    let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend.clone();
    let mut server = Server::from_runtime_parts(pty_backend, runtime_event_receiver);

    assert!(!server.has_active_panes(), "a fresh server parks no pane");

    let pane_id = PaneId::new();
    fake_pty_backend
        .spawn_pane(
            pane_id,
            SpawnSpec::build_default_shell(None, BTreeMap::new()),
            TEST_PANE_SIZE,
        )
        .expect("spawn");
    server.park_pane_pty(pane_id, TEST_PANE_SIZE);

    fake_pty_backend
        .push_output(pane_id, b"hi".to_vec())
        .expect("push");
    match server
        .get_inbox_receiver()
        .recv_timeout(SERVER_TEST_DEADLINE_DURATION)
    {
        Ok(RuntimeEvent::PtyOutput {
            pane_id: reported_pane_id,
            output_bytes,
        }) => {
            assert_eq!(reported_pane_id, pane_id);
            assert_eq!(output_bytes, b"hi");
        }
        received_event => panic!("expected PtyOutput, got {received_event:?}"),
    }

    assert!(server.has_active_panes());

    server.shutdown();

    assert_eq!(
        fake_pty_backend
            .list_pane_kill_policies(pane_id)
            .expect("pane"),
        vec![KillPolicy::GracefulTree {
            timeout_duration: GRACEFUL_TIMEOUT_DURATION,
        }]
    );
}
