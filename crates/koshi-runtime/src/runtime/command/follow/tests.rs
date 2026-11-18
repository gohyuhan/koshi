//! Tests for follow: the refusals of a follow request, the order in which
//! `NextPeer` steps through the peers, the parts of the leader's view a
//! follow copies and the events each copy emits, a chain of follows copied in
//! one event, the moves of the follower's input that the input guard counts,
//! and each act that ends a follow or leaves it standing.

use std::sync::mpsc;
use std::time::SystemTime;

use koshi_core::command::{
    ClosePaneArgs, FocusPaneArgs, FocusTarget, NewTabArgs, ScrollPaneArgs, ToggleLockModeArgs,
};
use koshi_core::geometry::Point;
use koshi_core::key::{BindingModifierFlags, Key, KeyChord};
use koshi_core::lock::LockMode;
use koshi_core::mouse::{MouseButton, MouseKind};
use koshi_ipc::protocol::WireMouseAction;
use koshi_session::client::FloatingPaneFocus;
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::{build_key_input_for_chord, build_new_pane_args};

use super::*;
use crate::runtime::command::tests::{
    attach_client_through_server, build_floating_new_pane_args, build_runtime_with_fake,
    create_session_with_attached_client, get_command_outcome, get_floating_pane_id_holding,
    get_only_session_slot,
};
use crate::runtime::delivery_queue::DeliveryReceiver;

/// The viewport of every client of [`build_follow_fixture`]: pane area `80x22`.
const VIEWPORT_SIZE: Size = Size {
    column_count: 80,
    row_count: 24,
};

/// Alice and Bob share the one tab of an `80x24` session. Alice split the
/// tab's left pane and types in the new right pane, and Bob types in the left
/// pane. The left pane holds 30 lines of output, so a view of it can scroll
/// back. Both clients subscribe.
struct FollowFixture {
    server: Server,
    fake_pty_backend: Arc<FakePtyBackend>,
    session_id: SessionId,
    tab_id: TabId,
    left_pane_id: PaneId,
    right_pane_id: PaneId,
    alice_client_id: ClientId,
    bob_client_id: ClientId,
    _delivery_receivers: [DeliveryReceiver; 2],
}

fn build_follow_fixture() -> FollowFixture {
    let (mut server, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let alice_client_id = create_session_with_attached_client(
        &mut server,
        SessionId::new(),
        VIEWPORT_SIZE,
        SystemTime::UNIX_EPOCH,
    )
    .expect("bootstrap alice");
    let (session_id, tab_id, left_pane_id) = get_only_session_slot(&server);
    let bob_client_id =
        attach_client_through_server(&mut server, alice_client_id, VIEWPORT_SIZE, None);
    let right_pane_id = find_created_pane_id(&run_command(
        &mut server,
        CommandSource::from_key_binding(alice_client_id),
        Command::NewPane(build_new_pane_args()),
    ));
    let _ = run_command(
        &mut server,
        CommandSource::from_key_binding(bob_client_id),
        build_focus_pane_command(left_pane_id, None),
    );
    server.handle_pty_output(left_pane_id, "line\r\n".repeat(30).as_bytes());
    let (alice_delivery_receiver, _) = server.subscribe(alice_client_id, 60, None);
    let (bob_delivery_receiver, _) = server.subscribe(bob_client_id, 60, None);
    FollowFixture {
        server,
        fake_pty_backend,
        session_id,
        tab_id,
        left_pane_id,
        right_pane_id,
        alice_client_id,
        bob_client_id,
        _delivery_receivers: [alice_delivery_receiver, bob_delivery_receiver],
    }
}

/// Run `command` from `command_source` as one inbox event
/// ([`Server::handle_runtime_event`]) and return its result.
fn run_command(
    server: &mut Server,
    command_source: CommandSource,
    command: Command,
) -> CommandResult {
    let (response_sender, response_receiver) = mpsc::channel();
    let _ = server.handle_runtime_event(RuntimeEvent::Ipc {
        command_envelope: Box::new(CommandEnvelope::from_parts(
            CommandId::new(),
            command_source,
            command,
        )),
        fired_action: None,
        response_sender,
    });
    response_receiver.recv().expect("the command result")
}

/// Run a [`Command::FollowClient`] of `follow_target` from
/// `follower_client_id`'s own key binding.
fn follow_client(
    server: &mut Server,
    follower_client_id: ClientId,
    follow_target: FollowTarget,
) -> CommandResult {
    run_command(
        server,
        CommandSource::from_key_binding(follower_client_id),
        Command::FollowClient(FollowClientArgs {
            follow_target,
            client_id: None,
        }),
    )
}

/// A [`Command::FocusPane`] of `pane_id` for `client_id`.
fn build_focus_pane_command(pane_id: PaneId, client_id: Option<ClientId>) -> Command {
    Command::FocusPane(FocusPaneArgs {
        focus_target: FocusTarget::Pane(pane_id),
        client_id,
    })
}

/// Scroll the view of `pane_id` by `scroll_line_count` lines from
/// `client_id`'s own key binding.
fn scroll_pane(server: &mut Server, client_id: ClientId, pane_id: PaneId, scroll_line_count: i32) {
    let command_result = run_command(
        server,
        CommandSource::from_key_binding(client_id),
        Command::ScrollPane(ScrollPaneArgs {
            pane_id: Some(pane_id),
            scroll_line_count,
        }),
    );
    assert_eq!(get_command_outcome(&command_result), Ok(Vec::new()));
}

/// The pane the first [`Event::PaneCreated`] of `command_result` names.
fn find_created_pane_id(command_result: &CommandResult) -> PaneId {
    let CommandResult::Ok { emitted_events, .. } = command_result else {
        panic!("expected a created pane, got {command_result:?}");
    };
    emitted_events
        .iter()
        .find_map(|emitted_event| match emitted_event {
            Event::PaneCreated(pane_created) => Some(pane_created.pane_id),
            _ => None,
        })
        .expect("the command created a pane")
}

/// Create a floating pane from `client_id`'s own key binding. Returns the pane
/// and the floating pane that holds it.
fn create_floating_pane_for(server: &mut Server, client_id: ClientId) -> (PaneId, FloatingPaneId) {
    let pane_id = find_created_pane_id(&run_command(
        server,
        CommandSource::from_key_binding(client_id),
        Command::NewPane(build_floating_new_pane_args(None, None, false)),
    ));
    (pane_id, get_floating_pane_id_holding(server, pane_id))
}

/// The client `client_id` in the session that holds it.
fn get_client(server: &Server, client_id: ClientId) -> &Client {
    server
        .get_session_for_client(client_id)
        .and_then(|session| session.clients.get_client_by_id(client_id))
        .expect("the client is attached")
}

/// The number of events the server published so far.
fn count_published_events(server: &Server) -> usize {
    server.recent_event_ring.list_recent_events().len()
}

/// The names of the events the server published after its first
/// `published_event_count` events.
fn list_event_names_after(server: &Server, published_event_count: usize) -> Vec<String> {
    server
        .recent_event_ring
        .list_recent_events()
        .into_iter()
        .skip(published_event_count)
        .map(|recent_event| recent_event.event_name.to_string())
        .collect()
}

/// A key press of `character` from `client_id`, typed against a frame that
/// carries `painted_input_pane_moved_by_others_count`.
fn build_key_press_event(
    client_id: ClientId,
    character: char,
    painted_input_pane_moved_by_others_count: u64,
) -> RuntimeEvent {
    RuntimeEvent::ClientKeyboard {
        client_id,
        key_inputs: vec![build_key_input_for_chord(KeyChord::from_parts(
            BindingModifierFlags::NONE,
            Key::Char(character),
        ))],
        painted_input_pane_moved_by_others_count,
    }
}

/// Alice zooms the right pane, scrolls the left pane back 3 lines and opens
/// the floating pane `htop`. Bob's follow gives Bob all of it: the tab, the
/// focus history `[right, left]`, the zoom, `htop` shown and focused, and the
/// offset `3`.
#[test]
fn a_follow_copies_the_leaders_tab_focus_zoom_floating_panes_and_scroll_offsets() {
    let FollowFixture {
        mut server,
        tab_id,
        left_pane_id,
        right_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let _ = run_command(
        &mut server,
        CommandSource::from_key_binding(alice_client_id),
        Command::TogglePaneFullscreen,
    );
    scroll_pane(&mut server, alice_client_id, left_pane_id, 3);
    let (htop_pane_id, htop_floating_pane_id) =
        create_floating_pane_for(&mut server, alice_client_id);

    let command_result = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    assert_eq!(get_command_outcome(&command_result), Ok(Vec::new()));
    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.get_followed_client_id(),
            bob.get_active_tab_id(),
            bob.list_focus_history(tab_id).to_vec(),
            bob.get_zoomed_pane_id(tab_id),
            bob.list_shown_floating_pane_ids().to_vec(),
            bob.get_scroll_offset(left_pane_id),
            bob.get_active_focused_pane_id(),
        ),
        (
            Some(alice_client_id),
            tab_id,
            vec![right_pane_id, left_pane_id],
            Some(right_pane_id),
            vec![htop_floating_pane_id],
            3,
            Some(htop_pane_id),
        )
    );
    let session = server
        .get_session_for_client(bob_client_id)
        .expect("bob's session");
    let followed_pane_ids = list_followed_pane_ids(session, tab_id);
    assert_eq!(
        bob.build_followed_view(&followed_pane_ids),
        get_client(&server, alice_client_id).build_followed_view(&followed_pane_ids)
    );
}

/// Alice opens a second tab. Bob's follow moves Bob there: one
/// `TabFocused`, then the `PaneFocused` of Bob's input.
#[test]
fn a_follow_of_a_leader_on_another_tab_switches_the_followers_tab() {
    let FollowFixture {
        mut server,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let _ = run_command(
        &mut server,
        CommandSource::from_key_binding(alice_client_id),
        Command::NewTab(NewTabArgs {
            working_directory: None,
            client_id: None,
        }),
    );
    let alice = get_client(&server, alice_client_id);
    let (second_tab_id, second_tab_pane_id) = (
        alice.get_active_tab_id(),
        alice.get_active_focused_pane_id(),
    );
    let published_event_count = count_published_events(&server);

    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    assert_eq!(
        list_event_names_after(&server, published_event_count),
        vec!["TabFocused", "PaneFocused"]
    );
    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (bob.get_active_tab_id(), bob.get_active_focused_pane_id()),
        (second_tab_id, second_tab_pane_id)
    );
}

/// Bob follows Alice. Alice scrolls the left pane back 4 lines: Bob's view
/// scrolls back 4 lines too, no event is published, and no PTY resizes.
#[test]
fn a_copy_of_a_scroll_offset_alone_publishes_no_event_and_resizes_no_pane() {
    let FollowFixture {
        mut server,
        fake_pty_backend,
        left_pane_id,
        right_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );
    let published_event_count = count_published_events(&server);
    let list_pty_sizes = |fake_pty_backend: &FakePtyBackend| {
        [left_pane_id, right_pane_id].map(|pane_id| {
            fake_pty_backend
                .list_pane_sizes(pane_id)
                .expect("the pane's sizes")
        })
    };
    let pty_sizes_before = list_pty_sizes(&fake_pty_backend);

    scroll_pane(&mut server, alice_client_id, left_pane_id, 4);

    assert_eq!(
        get_client(&server, bob_client_id).get_scroll_offset(left_pane_id),
        4
    );
    assert_eq!(
        list_event_names_after(&server, published_event_count),
        Vec::<&str>::new()
    );
    assert_eq!(list_pty_sizes(&fake_pty_backend), pty_sizes_before);
}

/// A follow of the follower itself, of a client the session does not hold,
/// of a client that is reconnecting, and of a client that follows the
/// follower is refused with its own help text, and Bob follows nobody.
#[test]
fn a_follow_of_itself_an_absent_client_a_reconnecting_client_or_a_loop_is_refused() {
    let FollowFixture {
        mut server,
        session_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let absent_client_id = ClientId::new();
    let session_name = server.session_by_id[&session_id].session_name.clone();

    let self_follow_result = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(bob_client_id),
    );
    let absent_follow_result = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(absent_client_id),
    );
    server.client_ids_awaiting_reconnect.insert(alice_client_id);
    let reconnecting_follow_result = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );
    server
        .client_ids_awaiting_reconnect
        .remove(&alice_client_id);
    let _ = follow_client(
        &mut server,
        alice_client_id,
        FollowTarget::Client(bob_client_id),
    );
    let loop_follow_result = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let self_follow_help = format!("{bob_client_id} cannot follow itself");
    let absent_follow_help =
        format!("client {absent_client_id} is not attached to session `{session_name}`");
    let reconnecting_follow_help = format!("client {alice_client_id} is reconnecting");
    let loop_follow_help =
        format!("following {alice_client_id} makes a loop: it follows {bob_client_id}");
    assert_eq!(
        [
            get_command_outcome(&self_follow_result),
            get_command_outcome(&absent_follow_result),
            get_command_outcome(&reconnecting_follow_result),
            get_command_outcome(&loop_follow_result),
        ],
        [
            Err((RejectReason::InvalidState, Some(self_follow_help.as_str()))),
            Err((
                RejectReason::TargetNotFound,
                Some(absent_follow_help.as_str())
            )),
            Err((
                RejectReason::InvalidState,
                Some(reconnecting_follow_help.as_str())
            )),
            Err((RejectReason::InvalidState, Some(loop_follow_help.as_str()))),
        ]
    );
    assert_eq!(
        get_client(&server, bob_client_id).get_followed_client_id(),
        None
    );
}

/// Alice (index `0`) shares the session with Bob (`1`) and Carol (`2`).
/// Three `NextPeer` follows make Alice follow Bob, then Carol, then nobody.
#[test]
fn a_next_peer_follow_steps_through_the_peers_in_color_order_then_ends() {
    let FollowFixture {
        mut server,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let carol_client_id =
        attach_client_through_server(&mut server, alice_client_id, VIEWPORT_SIZE, None);

    let followed_client_ids: Vec<Option<ClientId>> = (0..3)
        .map(|_| {
            let command_result =
                follow_client(&mut server, alice_client_id, FollowTarget::NextPeer);
            assert_eq!(get_command_outcome(&command_result), Ok(Vec::new()));
            get_client(&server, alice_client_id).get_followed_client_id()
        })
        .collect();

    assert_eq!(
        followed_client_ids,
        vec![Some(bob_client_id), Some(carol_client_id), None]
    );
}

/// Bob follows Alice. Alice's `NextPeer` follow skips Bob, who would make a
/// loop, and follows Carol. Once Carol is reconnecting, Alice, who follows
/// nobody, has no peer left: `no other client to follow`.
#[test]
fn a_next_peer_follow_skips_a_peer_that_makes_a_loop_or_is_reconnecting() {
    let FollowFixture {
        mut server,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let carol_client_id =
        attach_client_through_server(&mut server, alice_client_id, VIEWPORT_SIZE, None);
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let _ = follow_client(&mut server, alice_client_id, FollowTarget::NextPeer);
    assert_eq!(
        get_client(&server, alice_client_id).get_followed_client_id(),
        Some(carol_client_id)
    );

    let _ = follow_client(&mut server, alice_client_id, FollowTarget::Nobody);
    server.client_ids_awaiting_reconnect.insert(carol_client_id);
    let command_result = follow_client(&mut server, alice_client_id, FollowTarget::NextPeer);

    assert_eq!(
        get_command_outcome(&command_result),
        Err((
            RejectReason::InvalidState,
            Some("no other client to follow")
        ))
    );
    assert_eq!(
        get_client(&server, alice_client_id).get_followed_client_id(),
        None
    );
}

/// Carol follows Bob, and Bob follows Alice. Alice opens a second tab: in that
/// one event Bob's view copies Alice's, then Carol's view copies Bob's.
#[test]
fn a_chain_of_follows_copies_leader_first_in_one_event() {
    let FollowFixture {
        mut server,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let carol_client_id =
        attach_client_through_server(&mut server, alice_client_id, VIEWPORT_SIZE, None);
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );
    let _ = follow_client(
        &mut server,
        carol_client_id,
        FollowTarget::Client(bob_client_id),
    );

    let _ = run_command(
        &mut server,
        CommandSource::from_key_binding(alice_client_id),
        Command::NewTab(NewTabArgs {
            working_directory: None,
            client_id: None,
        }),
    );

    let alice = get_client(&server, alice_client_id);
    let alice_view = (
        alice.get_active_tab_id(),
        alice.get_active_focused_pane_id(),
    );
    let list_view = |client_id: ClientId| {
        let client = get_client(&server, client_id);
        (
            client.get_active_tab_id(),
            client.get_active_focused_pane_id(),
        )
    };
    assert_eq!(
        [list_view(bob_client_id), list_view(carol_client_id)],
        [alice_view, alice_view]
    );
}

/// Bob follows Alice: the start copy moves Bob's input from the left pane to
/// the right pane, and counts one move by others. Fifty scrolls of Alice's
/// view copy onto Bob's view and count nothing. Bob's key, typed against the
/// frame that carries the count `1`, reaches the right pane and ends the
/// follow.
#[test]
fn the_start_copy_counts_a_move_by_others_and_copied_scrolls_count_nothing() {
    let FollowFixture {
        mut server,
        fake_pty_backend,
        left_pane_id,
        right_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();

    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );
    assert_eq!(
        get_client(&server, bob_client_id).get_input_pane_moved_by_others_count(),
        1
    );
    for scroll_index in 0..50 {
        let scroll_line_count = if scroll_index % 2 == 0 { 1 } else { -1 };
        scroll_pane(
            &mut server,
            alice_client_id,
            left_pane_id,
            scroll_line_count,
        );
    }
    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.get_input_pane_moved_by_others_count(),
            bob.get_dropped_input_request_count(),
            bob.get_followed_client_id(),
        ),
        (1, 0, Some(alice_client_id))
    );

    let _ = server.handle_runtime_event(build_key_press_event(bob_client_id, 'x', 1));

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(right_pane_id)
            .expect("the right pane's writes"),
        vec![b"x".to_vec()]
    );
    assert_eq!(
        get_client(&server, bob_client_id).get_followed_client_id(),
        None
    );
}

/// Bob's key, typed against the frame from before the start copy moved his
/// input, reaches no pane, counts one dropped request, and leaves the follow
/// standing.
#[test]
fn a_key_typed_against_a_frame_from_before_the_start_copy_is_dropped_and_keeps_the_follow() {
    let FollowFixture {
        mut server,
        fake_pty_backend,
        left_pane_id,
        right_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let _ = server.handle_runtime_event(build_key_press_event(bob_client_id, 'y', 0));

    assert_eq!(
        [left_pane_id, right_pane_id].map(|pane_id| fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane's writes")),
        [Vec::<Vec<u8>>::new(), Vec::new()]
    );
    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.get_dropped_input_request_count(),
            bob.get_followed_client_id()
        ),
        (1, Some(alice_client_id))
    );
}

/// Bob follows Alice and focuses the left pane with his own key binding. The
/// focus moves, the follow ends, and Alice's next scroll stays off Bob's view.
#[test]
fn the_followers_own_focus_move_applies_then_ends_the_follow() {
    let FollowFixture {
        mut server,
        left_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let _ = run_command(
        &mut server,
        CommandSource::from_key_binding(bob_client_id),
        build_focus_pane_command(left_pane_id, None),
    );
    scroll_pane(&mut server, alice_client_id, left_pane_id, 2);

    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.get_followed_client_id(),
            bob.get_active_focused_pane_id(),
            bob.get_scroll_offset(left_pane_id),
        ),
        (None, Some(left_pane_id), 0)
    );
}

/// A CLI command that targets Bob scrolls Bob's view of the left pane back
/// 2 lines, and a CLI command whose `client_id` names Bob moves Bob's focus to
/// the left pane. Each change applies, and each ends Bob's follow.
#[test]
fn a_cli_command_that_names_the_follower_changes_its_view_and_ends_the_follow() {
    let FollowFixture {
        mut server,
        session_id,
        left_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let target_result = run_command(
        &mut server,
        CommandSource::from_external_cli(Some(session_id), Some(bob_client_id)),
        Command::ScrollPane(ScrollPaneArgs {
            pane_id: Some(left_pane_id),
            scroll_line_count: 2,
        }),
    );

    assert_eq!(get_command_outcome(&target_result), Ok(Vec::new()));
    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.get_followed_client_id(),
            bob.get_scroll_offset(left_pane_id)
        ),
        (None, 2)
    );

    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );
    let argument_result = run_command(
        &mut server,
        CommandSource::from_external_cli(Some(session_id), None),
        build_focus_pane_command(left_pane_id, Some(bob_client_id)),
    );

    assert_eq!(
        get_command_outcome(&argument_result),
        Ok(vec!["PaneFocused"])
    );
    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.get_followed_client_id(),
            bob.get_active_focused_pane_id()
        ),
        (None, Some(left_pane_id))
    );
}

/// Bob follows Alice. Bob's own lock toggle locks Bob, and his refused
/// follow of himself changes nothing: both leave the follow standing.
#[test]
fn a_lock_toggle_or_a_refused_command_of_the_follower_keeps_the_follow() {
    let FollowFixture {
        mut server,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let _ = run_command(
        &mut server,
        CommandSource::from_key_binding(bob_client_id),
        Command::ToggleLockMode(ToggleLockModeArgs { client_id: None }),
    );
    let refused_result = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(bob_client_id),
    );

    let self_follow_help = format!("{bob_client_id} cannot follow itself");
    assert_eq!(
        get_command_outcome(&refused_result),
        Err((RejectReason::InvalidState, Some(self_follow_help.as_str())))
    );
    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (bob.get_lock_mode(), bob.get_followed_client_id()),
        (LockMode::Locked, Some(alice_client_id))
    );
}

/// Bob follows Alice. Alice closes the right pane that both of them type in:
/// Bob's input moves to the left pane with Alice's, and the follow stands.
/// Then Alice detaches, and Bob's follow ends.
#[test]
fn the_leader_closing_a_shared_pane_keeps_the_follow_and_the_leader_leaving_ends_it() {
    let FollowFixture {
        mut server,
        tab_id,
        left_pane_id,
        right_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let _ = run_command(
        &mut server,
        CommandSource::from_key_binding(alice_client_id),
        Command::ClosePane(ClosePaneArgs {
            pane_id: Some(right_pane_id),
            should_force_close: false,
            should_kill_process_tree: false,
        }),
    );

    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.get_followed_client_id(),
            bob.list_focus_history(tab_id).to_vec()
        ),
        (Some(alice_client_id), vec![left_pane_id])
    );

    let _ = server.handle_client_detach(alice_client_id);

    assert_eq!(
        get_client(&server, bob_client_id).get_followed_client_id(),
        None
    );
}

/// Bob, on a `60x20` viewport, follows Alice onto her second tab. The tab
/// then solves at the smaller pane area, `60x18`, and its pane's PTY resizes
/// to it.
#[test]
fn a_smaller_follower_moved_onto_the_leaders_tab_shrinks_that_tab() {
    let (mut server, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let alice_client_id = create_session_with_attached_client(
        &mut server,
        SessionId::new(),
        VIEWPORT_SIZE,
        SystemTime::UNIX_EPOCH,
    )
    .expect("bootstrap alice");
    let (session_id, _first_tab_id, _first_pane_id) = get_only_session_slot(&server);
    let bob_client_id = attach_client_through_server(
        &mut server,
        alice_client_id,
        Size {
            column_count: 60,
            row_count: 20,
        },
        None,
    );
    let _ = run_command(
        &mut server,
        CommandSource::from_key_binding(alice_client_id),
        Command::NewTab(NewTabArgs {
            working_directory: None,
            client_id: None,
        }),
    );
    let alice = get_client(&server, alice_client_id);
    let second_tab_id = alice.get_active_tab_id();
    let second_tab_pane_id = alice
        .get_active_focused_pane_id()
        .expect("alice focuses the second tab's pane");

    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let session = &server.session_by_id[&session_id];
    let expected_pty_size = compute_pane_spawn_sizes(
        session.tabs[&second_tab_id].get_layout_tree(),
        Size {
            column_count: 60,
            row_count: 18,
        },
        session.get_pane_sizing(),
    )
    .into_iter()
    .find(|(solved_pane_id, _)| *solved_pane_id == second_tab_pane_id)
    .map(|(_, pty_size)| pty_size)
    .expect("the pane is solved");
    assert_eq!(
        get_client(&server, bob_client_id).get_active_tab_id(),
        second_tab_id
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_sizes(second_tab_pane_id)
            .expect("the pane's sizes")
            .last(),
        Some(&expected_pty_size)
    );
}

/// Alice shows the floating pane `htop`. Bob pinned `htop` at `(4, 2)`, then
/// minimized it. Bob's follow shows `htop` again with one
/// `FloatingPaneMinimizedChanged`, then the `PaneFocused` of Bob's input,
/// and Bob's pinned cell stays.
#[test]
fn a_follow_shows_a_floating_pane_the_follower_minimized_at_the_followers_own_cell() {
    let FollowFixture {
        mut server,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let (htop_pane_id, htop_floating_pane_id) =
        create_floating_pane_for(&mut server, alice_client_id);
    {
        let bob = server.get_client_mut(bob_client_id).expect("bob");
        bob.pin_floating_pane(htop_floating_pane_id, Point { column: 4, row: 2 });
        let _ = bob.minimize_floating_pane(htop_floating_pane_id);
    }
    assert_eq!(
        get_client(&server, bob_client_id).list_shown_floating_pane_ids(),
        []
    );
    let published_event_count = count_published_events(&server);

    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    assert_eq!(
        list_event_names_after(&server, published_event_count),
        vec!["FloatingPaneMinimizedChanged", "PaneFocused"]
    );
    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.list_shown_floating_pane_ids().to_vec(),
            bob.get_active_focused_pane_id(),
            bob.get_floating_pane_view(htop_floating_pane_id)
                .map(|floating_pane_view| floating_pane_view.position),
        ),
        (
            vec![htop_floating_pane_id],
            Some(htop_pane_id),
            Some(FloatingPanePosition::Pinned(Point { column: 4, row: 2 })),
        )
    );
}

/// Alice split the floating pane `htop` into `[htop | vim]`, focuses `vim`
/// and zooms it there. Bob's follow gives Bob the focus history
/// `[vim, htop]` and the zoom on `vim` inside that floating pane.
#[test]
fn a_follow_copies_the_focus_and_zoom_inside_a_floating_pane() {
    let FollowFixture {
        mut server,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let (htop_pane_id, htop_floating_pane_id) =
        create_floating_pane_for(&mut server, alice_client_id);
    let vim_pane_id = find_created_pane_id(&run_command(
        &mut server,
        CommandSource::from_key_binding(alice_client_id),
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Split {
                source_pane_id: Some(htop_pane_id),
                tab_id: None,
                direction: Direction::Right,
            },
            ..build_new_pane_args()
        }),
    ));
    server
        .get_client_mut(alice_client_id)
        .expect("alice")
        .zoom_pane_in_floating_pane(htop_floating_pane_id, vim_pane_id);

    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    assert_eq!(
        get_client(&server, bob_client_id)
            .build_followed_view(&[])
            .floating_pane_focus_by_floating_pane_id
            .get(&htop_floating_pane_id),
        Some(&FloatingPaneFocus {
            focus_history: vec![vim_pane_id, htop_pane_id],
            zoomed_pane_id: Some(vim_pane_id),
        })
    );
}

/// Bob follows Alice. Alice scrolls her view of the floating pane `htop`,
/// which holds 30 lines of output, back 2 lines: Bob's view of `htop`
/// scrolls back 2 lines too.
#[test]
fn a_scroll_of_a_pane_inside_a_floating_pane_is_copied() {
    let FollowFixture {
        mut server,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let (htop_pane_id, _htop_floating_pane_id) =
        create_floating_pane_for(&mut server, alice_client_id);
    server.handle_pty_output(htop_pane_id, "line\r\n".repeat(30).as_bytes());
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    scroll_pane(&mut server, alice_client_id, htop_pane_id, 2);

    assert_eq!(
        get_client(&server, bob_client_id).get_scroll_offset(htop_pane_id),
        2
    );
}

/// A mouse round from `client_id` that runs `mouse_actions`.
fn build_client_mouse_event(
    client_id: ClientId,
    request_id: u64,
    mouse_actions: Vec<WireMouseAction>,
) -> RuntimeEvent {
    RuntimeEvent::ClientMouse {
        client_id,
        request_id,
        mouse_actions,
    }
}

/// The program in the right pane runs on its alternate screen with alternate
/// scroll on, and asks for SGR mouse reports. Bob follows Alice. Bob's wheel
/// sends 2 up arrows to that program and ends his follow. Bob follows Alice
/// again, and his left click reaches the program as `ESC [ < 0 ; 1 ; 1 M` and
/// ends his follow too.
#[test]
fn wheel_arrows_or_a_mouse_report_that_reach_a_program_end_the_follow() {
    let FollowFixture {
        mut server,
        fake_pty_backend,
        right_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    server.handle_pty_output(
        right_pane_id,
        b"\x1b[?1049h\x1b[?1007h\x1b[?1000h\x1b[?1006h",
    );
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let _ = server.handle_runtime_event(build_client_mouse_event(
        bob_client_id,
        1,
        vec![WireMouseAction::AlternateScrollArrows {
            pane_id: right_pane_id,
            is_scrolling_up: true,
            arrow_count: 2,
        }],
    ));

    assert_eq!(
        get_client(&server, bob_client_id).get_followed_client_id(),
        None
    );
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let _ = server.handle_runtime_event(build_client_mouse_event(
        bob_client_id,
        2,
        vec![WireMouseAction::Forward {
            pane_id: right_pane_id,
            mouse_kind: MouseKind::Press(MouseButton::Left),
            modifier_flags: BindingModifierFlags::NONE,
            pane_local_cell: Point { column: 0, row: 0 },
        }],
    ));

    assert_eq!(
        get_client(&server, bob_client_id).get_followed_client_id(),
        None
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(right_pane_id)
            .expect("the right pane's writes"),
        vec![b"\x1b[A\x1b[A".to_vec(), b"\x1b[<0;1;1M".to_vec()]
    );
}

/// Bob follows Alice. Bob's wheel over the left pane, whose program runs on
/// its main screen, sends the program no arrow, and the follow stands.
#[test]
fn wheel_arrows_that_reach_no_program_keep_the_follow() {
    let FollowFixture {
        mut server,
        fake_pty_backend,
        left_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_follow_fixture();
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let _ = server.handle_runtime_event(build_client_mouse_event(
        bob_client_id,
        1,
        vec![WireMouseAction::AlternateScrollArrows {
            pane_id: left_pane_id,
            is_scrolling_up: true,
            arrow_count: 2,
        }],
    ));

    assert_eq!(
        (
            get_client(&server, bob_client_id).get_followed_client_id(),
            fake_pty_backend
                .list_pane_write_bytes(left_pane_id)
                .expect("the left pane's writes"),
        ),
        (Some(alice_client_id), Vec::<Vec<u8>>::new())
    );
}

/// Alice, on an `80x24` terminal, shows the floating pane `htop` on the
/// one-pane tab she shares with Bob. Bob's `4x24` terminal has no room for
/// floating panes. Both clients subscribe.
struct RoomlessFollowerFixture {
    server: Server,
    fake_pty_backend: Arc<FakePtyBackend>,
    tab_id: TabId,
    tab_pane_id: PaneId,
    htop_pane_id: PaneId,
    htop_floating_pane_id: FloatingPaneId,
    alice_client_id: ClientId,
    bob_client_id: ClientId,
    _delivery_receivers: [DeliveryReceiver; 2],
}

fn build_roomless_follower_fixture() -> RoomlessFollowerFixture {
    let (mut server, fake_pty_backend, _runtime_event_sender) = build_runtime_with_fake();
    let alice_client_id = create_session_with_attached_client(
        &mut server,
        SessionId::new(),
        VIEWPORT_SIZE,
        SystemTime::UNIX_EPOCH,
    )
    .expect("bootstrap alice");
    let (_session_id, tab_id, tab_pane_id) = get_only_session_slot(&server);
    let (htop_pane_id, htop_floating_pane_id) =
        create_floating_pane_for(&mut server, alice_client_id);
    let bob_client_id = attach_client_through_server(
        &mut server,
        alice_client_id,
        Size {
            column_count: 4,
            row_count: 24,
        },
        None,
    );
    let (alice_delivery_receiver, _) = server.subscribe(alice_client_id, 60, None);
    let (bob_delivery_receiver, _) = server.subscribe(bob_client_id, 60, None);
    assert_eq!(
        get_client(&server, alice_client_id).list_shown_floating_pane_ids(),
        [htop_floating_pane_id]
    );
    RoomlessFollowerFixture {
        server,
        fake_pty_backend,
        tab_id,
        tab_pane_id,
        htop_pane_id,
        htop_floating_pane_id,
        alice_client_id,
        bob_client_id,
        _delivery_receivers: [alice_delivery_receiver, bob_delivery_receiver],
    }
}

/// Bob follows Alice. Bob shows no floating pane, and his view counts as
/// following Alice's. Alice scrolls the tab's pane back 2 lines: Bob's view
/// scrolls back 2 lines and still shows no floating pane. Bob's next key
/// reaches the tab's pane, not `htop`.
#[test]
fn a_follower_with_no_room_for_floating_panes_shows_none_of_the_leaders() {
    let RoomlessFollowerFixture {
        mut server,
        fake_pty_backend,
        tab_id,
        tab_pane_id,
        htop_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_roomless_follower_fixture();

    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let session = server
        .get_session_for_client(bob_client_id)
        .expect("bob's session");
    let followed_pane_ids = list_followed_pane_ids(session, tab_id);
    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.list_shown_floating_pane_ids().to_vec(),
            bob.is_view_followed(get_client(&server, alice_client_id), &followed_pane_ids),
        ),
        (Vec::<FloatingPaneId>::new(), true)
    );
    server.handle_pty_output(tab_pane_id, "line\r\n".repeat(30).as_bytes());
    scroll_pane(&mut server, alice_client_id, tab_pane_id, 2);
    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.list_shown_floating_pane_ids().to_vec(),
            bob.get_scroll_offset(tab_pane_id)
        ),
        (Vec::<FloatingPaneId>::new(), 2)
    );
    let painted_count = bob.get_input_pane_moved_by_others_count();
    let _ = server.handle_runtime_event(build_key_press_event(bob_client_id, 'z', painted_count));
    assert_eq!(
        [tab_pane_id, htop_pane_id].map(|pane_id| fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("the pane's writes")),
        [vec![b"z".to_vec()], Vec::new()]
    );
}

/// Bob follows Alice with no room for floating panes. Bob's terminal grows
/// to `80x24`: his follow stands, and he shows `htop` as Alice does.
#[test]
fn a_follower_that_gains_room_for_floating_panes_shows_the_leaders() {
    let RoomlessFollowerFixture {
        mut server,
        htop_floating_pane_id,
        alice_client_id,
        bob_client_id,
        ..
    } = build_roomless_follower_fixture();
    let _ = follow_client(
        &mut server,
        bob_client_id,
        FollowTarget::Client(alice_client_id),
    );

    let _ = server.handle_runtime_event(RuntimeEvent::Resize {
        client_id: bob_client_id,
        viewport_size: VIEWPORT_SIZE,
        pane_area: None,
        cell_size: None,
    });

    let bob = get_client(&server, bob_client_id);
    assert_eq!(
        (
            bob.get_followed_client_id(),
            bob.list_shown_floating_pane_ids().to_vec()
        ),
        (Some(alice_client_id), vec![htop_floating_pane_id])
    );
}
