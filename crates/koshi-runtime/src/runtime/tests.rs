//! Server-level integration: the pieces the event loop wires together across
//! the runtime submodules. A spawned pane's output reaches the inbox, the pane
//! reports active, and the graceful quit teardown group-kills it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::runtime::pty_inbox::InboxSink;
use koshi_client::mouse::MouseAction;
use koshi_client::Client as ViewerClient;
use koshi_core::command::{CommandEnvelope, CommandSource};
use koshi_core::constant::GRACEFUL_TIMEOUT_DURATION;
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, CommandId, PaneId};
use koshi_core::mouse::{MouseInput, MouseKind};
use koshi_core::process::{KillPolicy, PtySize, SpawnSpec};
use koshi_observability::cleanup::TerminalCleanupGuard;
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

/// The viewer half for `client_id` on an 80x24 viewport, on the stock
/// settings, subscribed to `server`.
pub(crate) fn build_viewer(server: &mut Server, client_id: ClientId) -> ViewerClient {
    ViewerClient::from_client_id_and_viewport_size(
        client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        server.subscribe(client_id),
        TerminalCleanupGuard::new(),
    )
}

/// One mouse event at `event_time`, the way the running binary delivers it:
/// the viewer takes its queued events, decides what the event means against
/// the frame it is looking at, and only what it decided reaches the session.
pub(crate) fn dispatch_mouse_input_at_time(
    server: &mut Server,
    viewer: &mut ViewerClient,
    mouse_input: MouseInput,
    event_time: Instant,
) {
    viewer.apply_events();
    let mouse_frame = build_mouse_frame(
        server
            .build_snapshot(viewer.get_client_id())
            .expect("render snapshot"),
    );
    let mouse_actions = viewer.handle_mouse(mouse_input, &mouse_frame, event_time);
    apply_mouse_actions(server, viewer, &mouse_frame, mouse_actions);
}

/// Runs every action the viewer decided, in order, the way the binary's loop
/// does. The actions `note_scroll_applied` returns after a scroll join the end
/// of the queue.
pub(crate) fn apply_mouse_actions(
    server: &mut Server,
    viewer: &mut ViewerClient,
    mouse_frame: &MouseFrame,
    mouse_actions: Vec<MouseAction>,
) {
    let client_id = viewer.get_client_id();
    let mut mouse_action_queue: VecDeque<MouseAction> = mouse_actions.into();
    while let Some(mouse_action) = mouse_action_queue.pop_front() {
        match mouse_action {
            MouseAction::Scroll {
                pane_id,
                is_scrolling_up,
                scroll_line_count,
            } => {
                let view_top_row_index =
                    server.scroll_pane_view(client_id, pane_id, is_scrolling_up, scroll_line_count);
                mouse_action_queue.extend(viewer.note_scroll_applied(
                    pane_id,
                    view_top_row_index,
                    mouse_frame,
                ));
            }
            MouseAction::Forward {
                pane_id,
                mouse_input,
            } => {
                let is_report_written =
                    server.forward_mouse_to_pane(client_id, pane_id, mouse_input);
                if let (true, MouseKind::Press(mouse_button)) =
                    (is_report_written, mouse_input.mouse_kind)
                {
                    viewer.note_press_forwarded(pane_id, mouse_button);
                }
            }
            MouseAction::AlternateScrollArrows {
                pane_id,
                is_scrolling_up,
                arrow_count,
            } => {
                server.write_alternate_scroll_arrows(pane_id, is_scrolling_up, arrow_count);
            }
            MouseAction::Resize {
                pane_id,
                border_side,
                resize_step,
                requested_cell_count,
            } => {
                let applied_cell_count = server.drag_resize(
                    client_id,
                    pane_id,
                    border_side,
                    resize_step,
                    requested_cell_count,
                );
                viewer.note_resize_applied(pane_id, border_side, resize_step, applied_cell_count);
            }
            MouseAction::Command(command) => {
                let command_envelope = CommandEnvelope::from_parts(
                    CommandId::new(),
                    CommandSource::from_mouse(client_id),
                    *command,
                );
                let _ = server.submit_command(command_envelope);
            }
        }
    }
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
