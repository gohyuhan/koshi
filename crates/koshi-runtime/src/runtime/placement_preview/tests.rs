//! Tests for pane placement previews: the preview a named target builds, the
//! panes its frames carry, and which session changes build it again and mark
//! the client's frame.

use super::*;

use std::sync::mpsc;
use std::time::{Instant, SystemTime};

use koshi_core::geometry::{PaneArea, DEFAULT_FLOATING_PANE_SIZE};
use koshi_layout::tree::LayoutNode;
use koshi_pane::pane::state::PaneRecord;
use koshi_pty::backend::state::PtyBackend;
use koshi_session::client::{ClientOrigin, ClientRegistry};
use koshi_session::session::state::FloatingMember;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::runtime::command::tests::{
    attach_client, attach_client_record, build_horizontal_split, build_tab,
};
use crate::runtime::delivery_queue::DeliveryReceiver;
use crate::runtime::event::RuntimeEvent;
use crate::runtime::frame::build_wire_placement_preview;
use crate::runtime::pty_inbox::InboxSink;
use crate::runtime::render_schedule::tests::{
    mark_changed_views_then_compute_frame_wait, subscribe_and_push_first_frame,
};
use koshi_ipc::placement::PanePlacementPreview as WirePlacementPreview;

/// A server with the tabs `[vim | top]` and `[log]` of one session. Alice and
/// Bob view the first tab, Alice typing in `vim` and Bob in `top`. Neither
/// reports a pane area: each tab solves at `80x22`.
struct PreviewFixture {
    server: Server,
    session_id: SessionId,
    alice_client_id: ClientId,
    bob_client_id: ClientId,
    source_tab_id: TabId,
    vim_pane_id: PaneId,
    top_pane_id: PaneId,
    destination_tab_id: TabId,
    log_pane_id: PaneId,
}

/// A server with no session, driving fake PTYs.
fn build_test_server() -> Server {
    let (runtime_event_sender, inbox_receiver) = mpsc::channel::<RuntimeEvent>();
    let pty_backend: Arc<dyn PtyBackend> = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(runtime_event_sender),
    )));
    Server::from_runtime_parts(pty_backend, inbox_receiver)
}

/// A session with no tab, no pane and no client.
fn build_empty_session() -> Session {
    Session::from_identity_and_client_registry(
        SessionId::new(),
        "s".to_string(),
        SystemTime::UNIX_EPOCH,
        ClientRegistry::new(),
    )
}

/// Register a pane record for each of `pane_ids` in `session`.
fn register_pane_records(session: &mut Session, pane_ids: &[PaneId]) {
    for &pane_id in pane_ids {
        session
            .panes
            .register_pane_record(PaneRecord::from_terminal_pane(pane_id))
            .expect("unique pane id");
    }
}

/// Build the [`PreviewFixture`] with every view change of its setup read.
fn build_preview_fixture() -> PreviewFixture {
    let mut server = build_test_server();
    let (alice_client_id, bob_client_id) = (ClientId::new(), ClientId::new());
    let (source_tab_id, destination_tab_id) = (TabId::new(), TabId::new());
    let (vim_pane_id, top_pane_id, log_pane_id) = (PaneId::new(), PaneId::new(), PaneId::new());
    let mut session = build_empty_session();
    register_pane_records(&mut session, &[vim_pane_id, top_pane_id, log_pane_id]);
    session.tabs.insert(
        source_tab_id,
        build_tab(
            source_tab_id,
            "work",
            0,
            build_horizontal_split(vim_pane_id, top_pane_id),
        ),
    );
    session.tabs.insert(
        destination_tab_id,
        build_tab(destination_tab_id, "logs", 1, LayoutNode::Pane(log_pane_id)),
    );
    attach_client(
        &mut session,
        alice_client_id,
        source_tab_id,
        Some(vim_pane_id),
    );
    attach_client(
        &mut session,
        bob_client_id,
        source_tab_id,
        Some(top_pane_id),
    );
    let session_id = session.session_id;
    server.session_by_id.insert(session_id, session);
    server.mark_changed_views();
    PreviewFixture {
        server,
        session_id,
        alice_client_id,
        bob_client_id,
        source_tab_id,
        vim_pane_id,
        top_pane_id,
        destination_tab_id,
        log_pane_id,
    }
}

impl PreviewFixture {
    /// Name Alice's preview of `vim` into the `[log]` tab, then subscribe
    /// Alice and push her first frame: no frame is owed after it.
    fn preview_vim_into_the_log_tab(&mut self, current_time: Instant) -> DeliveryReceiver {
        self.server.set_pane_placement_preview(
            self.alice_client_id,
            Some(PanePlacementPreviewTarget {
                source_pane_id: self.vim_pane_id,
                destination_tab_id: self.destination_tab_id,
            }),
        );
        subscribe_and_push_first_frame(&mut self.server, self.alice_client_id, current_time)
    }

    /// The preview Alice's frames carry.
    fn get_alice_placement_preview(&self) -> Arc<PlacementPreview> {
        Arc::clone(
            self.server.placement_preview_by_client_id[&self.alice_client_id]
                .get_placement_preview(),
        )
    }

    /// The panes Alice's frames carry for her preview.
    fn list_alice_preview_pane_ids(&self) -> Vec<PaneId> {
        self.server.placement_preview_by_client_id[&self.alice_client_id]
            .list_preview_pane_ids()
            .to_vec()
    }

    /// Mutable access to the fixture's session.
    fn get_session_mut(&mut self) -> &mut Session {
        self.server
            .session_by_id
            .get_mut(&self.session_id)
            .expect("the session exists")
    }
}

#[test]
fn a_named_target_builds_the_preview_of_both_tabs_and_marks_the_clients_frame() {
    let mut preview_fixture = build_preview_fixture();
    let current_time = Instant::now();
    let _delivery_receiver = subscribe_and_push_first_frame(
        &mut preview_fixture.server,
        preview_fixture.alice_client_id,
        current_time,
    );

    preview_fixture.server.set_pane_placement_preview(
        preview_fixture.alice_client_id,
        Some(PanePlacementPreviewTarget {
            source_pane_id: preview_fixture.vim_pane_id,
            destination_tab_id: preview_fixture.destination_tab_id,
        }),
    );

    let placement_preview = preview_fixture.get_alice_placement_preview();
    let placement_snapshot = placement_preview
        .find_placement_snapshot()
        .expect("the preview is available");
    assert_eq!(
        placement_snapshot
            .find_source_tab_snapshot()
            .map(|source_tab_snapshot| (
                source_tab_snapshot.tab_snapshot.tab_id,
                source_tab_snapshot.focus_history.clone()
            )),
        Some((
            preview_fixture.source_tab_id,
            vec![preview_fixture.vim_pane_id]
        ))
    );
    assert_eq!(
        placement_snapshot
            .destination_tab_snapshot
            .as_ref()
            .map(|destination_tab_snapshot| (
                destination_tab_snapshot.tab_snapshot.tab_id,
                destination_tab_snapshot.tab_snapshot.tab_size
            )),
        Some((
            preview_fixture.destination_tab_id,
            Size {
                column_count: 80,
                row_count: 22,
            }
        ))
    );
    let WirePlacementPreview::Available(wire_placement_snapshot) =
        build_wire_placement_preview(&placement_preview)
    else {
        panic!("an available preview travels as one");
    };
    assert_eq!(wire_placement_snapshot.validate(), Ok(()));
    assert_eq!(
        preview_fixture.list_alice_preview_pane_ids(),
        vec![preview_fixture.log_pane_id, preview_fixture.vim_pane_id],
        "the destination tab draws `log`, and every placement adds `vim`"
    );
    assert!(mark_changed_views_then_compute_frame_wait(
        &mut preview_fixture.server,
        preview_fixture.alice_client_id,
        current_time
    )
    .is_some());
}

#[test]
fn naming_the_target_the_client_already_names_changes_nothing() {
    let mut preview_fixture = build_preview_fixture();
    let current_time = Instant::now();
    let _delivery_receiver = preview_fixture.preview_vim_into_the_log_tab(current_time);
    let placement_preview_before = preview_fixture.get_alice_placement_preview();

    preview_fixture.server.set_pane_placement_preview(
        preview_fixture.alice_client_id,
        Some(PanePlacementPreviewTarget {
            source_pane_id: preview_fixture.vim_pane_id,
            destination_tab_id: preview_fixture.destination_tab_id,
        }),
    );

    assert!(Arc::ptr_eq(
        &preview_fixture.get_alice_placement_preview(),
        &placement_preview_before
    ));
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(
            &mut preview_fixture.server,
            preview_fixture.alice_client_id,
            current_time
        ),
        None
    );
}

#[test]
fn another_clients_scroll_keeps_the_preview_and_marks_no_frame() {
    let mut preview_fixture = build_preview_fixture();
    let current_time = Instant::now();
    let _delivery_receiver = preview_fixture.preview_vim_into_the_log_tab(current_time);
    let placement_preview_before = preview_fixture.get_alice_placement_preview();

    let top_pane_id = preview_fixture.top_pane_id;
    preview_fixture
        .server
        .get_client_mut(preview_fixture.bob_client_id)
        .expect("bob is attached")
        .set_scroll_offset(top_pane_id, 3);
    preview_fixture.server.mark_changed_views();

    assert!(Arc::ptr_eq(
        &preview_fixture.get_alice_placement_preview(),
        &placement_preview_before
    ));
    assert_eq!(
        mark_changed_views_then_compute_frame_wait(
            &mut preview_fixture.server,
            preview_fixture.alice_client_id,
            current_time
        ),
        None,
        "bob's scroll changes nothing alice's frame shows"
    );
}

#[test]
fn a_rebuilt_preview_equal_to_the_held_one_keeps_the_held_arc() {
    let mut preview_fixture = build_preview_fixture();
    let current_time = Instant::now();
    let _delivery_receiver = preview_fixture.preview_vim_into_the_log_tab(current_time);
    let placement_preview_before = preview_fixture.get_alice_placement_preview();
    let preview_pane_ids_before = preview_fixture.list_alice_preview_pane_ids();

    // A new tab changes the tab bar: every preview is built again, and this
    // one reads nothing the new tab changes.
    let new_tab_id = TabId::new();
    let new_pane_id = PaneId::new();
    let session = preview_fixture.get_session_mut();
    register_pane_records(session, &[new_pane_id]);
    let _ = session.add_tab(build_tab(
        new_tab_id,
        "new",
        2,
        LayoutNode::Pane(new_pane_id),
    ));
    preview_fixture.server.mark_changed_views();

    assert!(Arc::ptr_eq(
        &preview_fixture.get_alice_placement_preview(),
        &placement_preview_before
    ));
    assert_eq!(
        preview_fixture.list_alice_preview_pane_ids(),
        preview_pane_ids_before
    );
}

#[test]
fn a_layout_change_in_the_destination_tab_rebuilds_the_preview_and_marks_the_frame() {
    let mut preview_fixture = build_preview_fixture();
    let current_time = Instant::now();
    let _delivery_receiver = preview_fixture.preview_vim_into_the_log_tab(current_time);
    let placement_preview_before = preview_fixture.get_alice_placement_preview();

    let split_pane_id = PaneId::new();
    let (destination_tab_id, log_pane_id) = (
        preview_fixture.destination_tab_id,
        preview_fixture.log_pane_id,
    );
    let session = preview_fixture.get_session_mut();
    register_pane_records(session, &[split_pane_id]);
    let _ = session.update_tab_layout(
        destination_tab_id,
        build_horizontal_split(log_pane_id, split_pane_id),
    );
    preview_fixture.server.mark_changed_views();

    let placement_preview = preview_fixture.get_alice_placement_preview();
    assert!(!Arc::ptr_eq(&placement_preview, &placement_preview_before));
    assert_eq!(
        placement_preview
            .find_placement_snapshot()
            .and_then(|placement_snapshot| placement_snapshot.destination_tab_snapshot.as_ref())
            .map(|destination_tab_snapshot| destination_tab_snapshot
                .layout_tree
                .list_leaf_pane_ids()),
        Some(vec![log_pane_id, split_pane_id])
    );
    assert_eq!(
        preview_fixture.list_alice_preview_pane_ids(),
        vec![log_pane_id, split_pane_id, preview_fixture.vim_pane_id]
    );
    assert!(mark_changed_views_then_compute_frame_wait(
        &mut preview_fixture.server,
        preview_fixture.alice_client_id,
        current_time
    )
    .is_some());
}

#[test]
fn the_acting_clients_focus_move_rebuilds_the_preview_with_its_focus_history() {
    let mut preview_fixture = build_preview_fixture();
    let current_time = Instant::now();
    let _delivery_receiver = preview_fixture.preview_vim_into_the_log_tab(current_time);
    let placement_preview_before = preview_fixture.get_alice_placement_preview();

    let (source_tab_id, top_pane_id, vim_pane_id) = (
        preview_fixture.source_tab_id,
        preview_fixture.top_pane_id,
        preview_fixture.vim_pane_id,
    );
    preview_fixture
        .server
        .get_client_mut(preview_fixture.alice_client_id)
        .expect("alice is attached")
        .update_focused_pane(source_tab_id, top_pane_id);
    preview_fixture.server.mark_changed_views();

    let placement_preview = preview_fixture.get_alice_placement_preview();
    assert!(!Arc::ptr_eq(&placement_preview, &placement_preview_before));
    assert_eq!(
        placement_preview
            .find_placement_snapshot()
            .and_then(PlacementSnapshot::find_source_tab_snapshot)
            .map(|source_tab_snapshot| source_tab_snapshot.focus_history.clone()),
        Some(vec![top_pane_id, vim_pane_id])
    );
}

#[test]
fn a_detached_client_loses_its_preview() {
    let mut preview_fixture = build_preview_fixture();
    let current_time = Instant::now();
    let _delivery_receiver = preview_fixture.preview_vim_into_the_log_tab(current_time);

    let _ = preview_fixture
        .server
        .handle_client_detach(preview_fixture.alice_client_id);
    preview_fixture.server.mark_changed_views();

    assert!(!preview_fixture
        .server
        .placement_preview_by_client_id
        .contains_key(&preview_fixture.alice_client_id));
}

#[test]
fn an_attach_naming_a_placement_target_gives_its_first_frame_the_targets_preview() {
    let mut preview_fixture = build_preview_fixture();

    let attach_accepted = preview_fixture
        .server
        .handle_ipc_attach(
            None,
            None,
            Size {
                column_count: 80,
                row_count: 24,
            },
            None,
            None,
            SystemTime::UNIX_EPOCH,
            false,
            60,
            Some(PanePlacementPreviewTarget {
                source_pane_id: preview_fixture.vim_pane_id,
                destination_tab_id: preview_fixture.destination_tab_id,
            }),
        )
        .expect("the session takes the attach");
    let render_snapshot = preview_fixture
        .server
        .build_snapshot(attach_accepted.client_id)
        .expect("the attached client's first frame");

    let client_placement_preview =
        &preview_fixture.server.placement_preview_by_client_id[&attach_accepted.client_id];
    assert!(Arc::ptr_eq(
        render_snapshot
            .placement_preview
            .as_ref()
            .expect("the first frame carries the preview"),
        client_placement_preview.get_placement_preview()
    ));
    assert_eq!(
        client_placement_preview
            .get_placement_preview()
            .get_placement_target(),
        (
            preview_fixture.vim_pane_id,
            preview_fixture.destination_tab_id
        )
    );
    assert_eq!(
        render_snapshot
            .pane_snapshots
            .iter()
            .map(|pane_snapshot| pane_snapshot.pane_id)
            .collect::<Vec<PaneId>>(),
        vec![
            preview_fixture.vim_pane_id,
            preview_fixture.top_pane_id,
            preview_fixture.log_pane_id
        ]
    );
}

#[test]
fn an_attach_naming_no_placement_target_gives_the_client_no_preview() {
    let mut preview_fixture = build_preview_fixture();

    let attach_accepted = preview_fixture
        .server
        .handle_ipc_attach(
            None,
            None,
            Size {
                column_count: 80,
                row_count: 24,
            },
            None,
            None,
            SystemTime::UNIX_EPOCH,
            false,
            60,
            None,
        )
        .expect("the session takes the attach");

    assert!(!preview_fixture
        .server
        .placement_preview_by_client_id
        .contains_key(&attach_accepted.client_id));
    assert_eq!(
        preview_fixture
            .server
            .build_snapshot(attach_accepted.client_id)
            .expect("the attached client's first frame")
            .placement_preview,
        None
    );
}

#[test]
fn an_unavailable_preview_becomes_available_once_its_destination_tab_exists() {
    let mut preview_fixture = build_preview_fixture();
    let current_time = Instant::now();
    let _delivery_receiver = subscribe_and_push_first_frame(
        &mut preview_fixture.server,
        preview_fixture.alice_client_id,
        current_time,
    );
    let new_tab_id = TabId::new();
    preview_fixture.server.set_pane_placement_preview(
        preview_fixture.alice_client_id,
        Some(PanePlacementPreviewTarget {
            source_pane_id: preview_fixture.vim_pane_id,
            destination_tab_id: new_tab_id,
        }),
    );
    assert_eq!(
        *preview_fixture.get_alice_placement_preview(),
        PlacementPreview::Unavailable {
            source_pane_id: preview_fixture.vim_pane_id,
            destination_tab_id: new_tab_id,
        }
    );
    assert_eq!(preview_fixture.list_alice_preview_pane_ids(), Vec::new());

    let new_pane_id = PaneId::new();
    let session = preview_fixture.get_session_mut();
    register_pane_records(session, &[new_pane_id]);
    let _ = session.add_tab(build_tab(
        new_tab_id,
        "new",
        2,
        LayoutNode::Pane(new_pane_id),
    ));
    preview_fixture.server.mark_changed_views();

    assert_eq!(
        preview_fixture
            .get_alice_placement_preview()
            .find_placement_snapshot()
            .and_then(|placement_snapshot| placement_snapshot.destination_tab_snapshot.as_ref())
            .map(|destination_tab_snapshot| destination_tab_snapshot.tab_snapshot.tab_id),
        Some(new_tab_id)
    );
    assert_eq!(
        preview_fixture.list_alice_preview_pane_ids(),
        vec![new_pane_id, preview_fixture.vim_pane_id]
    );
}

#[test]
fn a_frame_carries_the_content_of_each_preview_pane_after_its_own_panes() {
    let mut preview_fixture = build_preview_fixture();
    let current_time = Instant::now();
    let _delivery_receiver = preview_fixture.preview_vim_into_the_log_tab(current_time);

    let render_snapshot = preview_fixture
        .server
        .build_snapshot(preview_fixture.alice_client_id)
        .expect("alice's frame");

    assert_eq!(
        render_snapshot
            .pane_snapshots
            .iter()
            .map(|pane_snapshot| pane_snapshot.pane_id)
            .collect::<Vec<PaneId>>(),
        vec![
            preview_fixture.vim_pane_id,
            preview_fixture.top_pane_id,
            preview_fixture.log_pane_id
        ]
    );
    assert!(Arc::ptr_eq(
        render_snapshot
            .placement_preview
            .as_ref()
            .expect("the frame carries the preview"),
        &preview_fixture.get_alice_placement_preview()
    ));
    assert!(preview_fixture
        .server
        .is_pane_shown_to_client(preview_fixture.alice_client_id, preview_fixture.log_pane_id));
    assert!(!preview_fixture
        .server
        .is_pane_shown_to_client(preview_fixture.bob_client_id, preview_fixture.log_pane_id));
}

#[test]
fn a_destination_tab_is_sized_by_the_acting_client_and_the_clients_that_view_it() {
    let mut server = build_test_server();
    let (acting_client_id, destination_viewer_id) = (ClientId::new(), ClientId::new());
    let (source_tab_id, destination_tab_id) = (TabId::new(), TabId::new());
    let (source_pane_id, destination_pane_id) = (PaneId::new(), PaneId::new());
    let mut session = build_empty_session();
    register_pane_records(&mut session, &[source_pane_id, destination_pane_id]);
    session.tabs.insert(
        source_tab_id,
        build_tab(source_tab_id, "work", 0, LayoutNode::Pane(source_pane_id)),
    );
    session.tabs.insert(
        destination_tab_id,
        build_tab(
            destination_tab_id,
            "logs",
            1,
            LayoutNode::Pane(destination_pane_id),
        ),
    );
    for (client_id, tab_id, pane_id, pane_area) in [
        (
            acting_client_id,
            source_tab_id,
            source_pane_id,
            Size {
                column_count: 80,
                row_count: 20,
            },
        ),
        (
            destination_viewer_id,
            destination_tab_id,
            destination_pane_id,
            Size {
                column_count: 120,
                row_count: 40,
            },
        ),
    ] {
        let mut client = Client::from_attachment(
            client_id,
            session.session_id,
            SystemTime::UNIX_EPOCH,
            Size {
                column_count: 240,
                row_count: 60,
            },
            Some(PaneArea::Reported(pane_area)),
            tab_id,
            ClientOrigin::Local,
            "C-test-client".to_string(),
            0,
        );
        client.update_focused_pane(tab_id, pane_id);
        attach_client_record(&mut session, client);
    }
    server.session_by_id.insert(session.session_id, session);

    server.set_pane_placement_preview(
        acting_client_id,
        Some(PanePlacementPreviewTarget {
            source_pane_id,
            destination_tab_id,
        }),
    );

    let placement_preview = Arc::clone(
        server.placement_preview_by_client_id[&acting_client_id].get_placement_preview(),
    );
    let placement_snapshot = placement_preview
        .find_placement_snapshot()
        .expect("the preview is available");
    // The acting client moves with its pane: `80x20` and `120x40` give `80x20`.
    assert_eq!(
        placement_snapshot
            .destination_tab_snapshot
            .as_ref()
            .map(|destination_tab_snapshot| destination_tab_snapshot.tab_snapshot.tab_size),
        Some(Size {
            column_count: 80,
            row_count: 20,
        })
    );
    // The source tab closes with its only pane: it is solved at `0x0`, and
    // only a placement in the destination tab draws the source pane.
    assert_eq!(
        server.placement_preview_by_client_id[&acting_client_id].list_preview_pane_ids(),
        [destination_pane_id, source_pane_id]
    );
}

#[test]
fn a_starving_sole_viewers_preview_draws_no_pane() {
    let mut server = build_test_server();
    let client_id = ClientId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut session = build_empty_session();
    register_pane_records(&mut session, &[pane_id]);
    session.tabs.insert(
        tab_id,
        build_tab(tab_id, "work", 0, LayoutNode::Pane(pane_id)),
    );
    let mut client = Client::from_attachment(
        client_id,
        session.session_id,
        SystemTime::UNIX_EPOCH,
        Size {
            column_count: 80,
            row_count: 24,
        },
        Some(PaneArea::Starving),
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    client.update_focused_pane(tab_id, pane_id);
    attach_client_record(&mut session, client);
    server.session_by_id.insert(session.session_id, session);

    server.set_pane_placement_preview(
        client_id,
        Some(PanePlacementPreviewTarget {
            source_pane_id: pane_id,
            destination_tab_id: tab_id,
        }),
    );

    let client_placement_preview = &server.placement_preview_by_client_id[&client_id];
    assert_eq!(
        client_placement_preview
            .get_placement_preview()
            .find_placement_snapshot()
            .and_then(PlacementSnapshot::find_source_tab_snapshot)
            .map(|source_tab_snapshot| source_tab_snapshot.tab_snapshot.is_every_pane_suppressed),
        Some(true)
    );
    assert_eq!(
        client_placement_preview.list_preview_pane_ids(),
        &[] as &[PaneId]
    );
}

#[test]
fn a_floating_source_previews_the_destination_tab_only() {
    let mut preview_fixture = build_preview_fixture();
    let floating_pane_id = PaneId::new();
    let session = preview_fixture.get_session_mut();
    register_pane_records(session, &[floating_pane_id]);
    session
        .add_floating_member(
            FloatingMember {
                pane_id: floating_pane_id,
                desired_size: DEFAULT_FLOATING_PANE_SIZE,
                solved_size: Size {
                    column_count: 48,
                    row_count: 13,
                },
            },
            &[],
        )
        .expect("the set has room");

    preview_fixture.server.set_pane_placement_preview(
        preview_fixture.alice_client_id,
        Some(PanePlacementPreviewTarget {
            source_pane_id: floating_pane_id,
            destination_tab_id: preview_fixture.source_tab_id,
        }),
    );

    let placement_preview = preview_fixture.get_alice_placement_preview();
    let placement_snapshot = placement_preview
        .find_placement_snapshot()
        .expect("the preview is available");
    assert_eq!(placement_snapshot.source, PlacementSource::Floating);
    assert_eq!(
        placement_snapshot
            .destination_tab_snapshot
            .as_ref()
            .map(|destination_tab_snapshot| destination_tab_snapshot.tab_snapshot.tab_id),
        Some(preview_fixture.source_tab_id)
    );
    assert_eq!(placement_snapshot.swap_tab_sizes, Vec::new());
    assert_eq!(
        preview_fixture.list_alice_preview_pane_ids(),
        vec![
            preview_fixture.vim_pane_id,
            preview_fixture.top_pane_id,
            floating_pane_id
        ]
    );
}
