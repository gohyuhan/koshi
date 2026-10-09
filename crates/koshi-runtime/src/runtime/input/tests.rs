//! End-to-end input tests through the viewer's keymap: keys passed through to
//! the pane, the lock escape key, keys held while a multi-chord binding is
//! incomplete, multi-chord dispatch, the timeout fallback, which pane a press
//! may reach, host paste, and pane resize.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc};

use crate::runtime::pty_inbox::InboxSink;
use crate::runtime::tests::build_viewer;
use koshi_config::conflict::KeymapVerdict;
use koshi_config::layer::{PartialKeybindingsConfig, PartialKoshiConfig, PartialLayoutDefaults};
use koshi_config::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::action::ActionReference;
use koshi_core::command::{
    Command, CommandResult, FocusPaneArgs, FocusTarget, NewPaneArgs, NewPanePlacement,
};
use koshi_core::geometry::{Direction, PaneArea, Size};
use koshi_core::ids::SessionId;
use koshi_core::key::{
    BindingModifierFlags, ExtendedKeysMode, Key, KeyChord, KeyEventKind, KeyIdentity,
    KeyModifierFlags, KeySequence, NamedKey,
};
use koshi_core::lock::LockMode;
use koshi_layout::edit::split_leaf;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_session::client::{Client, ClientOrigin};
use koshi_test_support::fake_pty::FakePtyBackend;
use koshi_test_support::fixtures::build_key_input_for_chord;
use std::time::{Duration, Instant};

use koshi_client::input::KeyOutcome;
use koshi_client::Client as ViewerClient;
use koshi_pty::error::PtyError;

use crate::server::Server;

use koshi_core::command::{CommandEnvelope, CommandSource};
use koshi_core::ids::CommandId;
use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action, DispatchPlan};
use std::time::SystemTime;

#[test]
fn recovery_notice_clears_only_after_pane_input_succeeds() {
    let (mut server, fake_pty_backend, client_id, _) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let session_id = server
        .get_session_for_client(client_id)
        .expect("attached session")
        .session_id;
    server.show_session_recovery_notice(session_id);
    assert!(
        server
            .build_snapshot(client_id)
            .expect("painted snapshot")
            .is_recovery_notice_visible
    );

    fake_pty_backend.fail_writes_on(pane_id, PtyError::UnknownPane { pane_id });
    server.handle_key_input(
        client_id,
        &build_key_input_for_chord(build_key_chord(BindingModifierFlags::NONE, 'x')),
    );
    assert!(
        server
            .build_snapshot(client_id)
            .expect("painted snapshot")
            .is_recovery_notice_visible
    );

    let (mut server, _, client_id, _) = build_test_server();
    let session_id = server
        .get_session_for_client(client_id)
        .expect("attached session")
        .session_id;
    server.show_session_recovery_notice(session_id);
    server.handle_key_input(
        client_id,
        &build_key_input_for_chord(build_key_chord(BindingModifierFlags::NONE, 'x')),
    );
    assert!(
        !server
            .build_snapshot(client_id)
            .expect("painted snapshot")
            .is_recovery_notice_visible
    );
}

#[test]
fn recovery_notice_stays_for_empty_or_failed_paste_and_clears_for_written_paste() {
    let (mut server, fake_pty_backend, client_id, _) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let session_id = server
        .get_session_for_client(client_id)
        .expect("attached session")
        .session_id;
    server.show_session_recovery_notice(session_id);

    server.handle_host_paste(client_id, "");
    assert!(
        server
            .build_snapshot(client_id)
            .expect("frame")
            .is_recovery_notice_visible
    );

    fake_pty_backend.fail_writes_on(pane_id, PtyError::UnknownPane { pane_id });
    server.handle_host_paste(client_id, "failed");
    assert!(
        server
            .build_snapshot(client_id)
            .expect("frame")
            .is_recovery_notice_visible
    );

    let (mut server, fake_pty_backend, client_id, _) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let session_id = server
        .get_session_for_client(client_id)
        .expect("attached session")
        .session_id;
    server.show_session_recovery_notice(session_id);
    server.handle_host_paste(client_id, "ready");
    assert_eq!(
        fake_pty_backend.list_pane_write_bytes(pane_id),
        Ok(vec![b"ready".to_vec()])
    );
    assert!(
        !server
            .build_snapshot(client_id)
            .expect("frame")
            .is_recovery_notice_visible
    );
}

impl Server {
    /// Run the action a viewer's keypress resolved to, the way an attached
    /// viewer does: resolve `bound_action.action_reference` against the
    /// built-in table with `new_pane_direction`, dispatch the command it
    /// stands for attributed to `client_id`'s keybinding, and mark the status
    /// line stale. A viewer-local action dispatches nothing and still marks the
    /// status line stale.
    ///
    /// An action that does not resolve dispatches nothing and marks nothing
    /// stale.
    fn handle_bound_action(
        &mut self,
        client_id: ClientId,
        bound_action: BoundAction,
        new_pane_direction: Direction,
    ) {
        let Ok(dispatch_plan) = resolve_action(
            &bound_action.action_reference,
            &ActionRegistry::new(),
            new_pane_direction,
        ) else {
            return;
        };
        if let DispatchPlan::Command(command) = dispatch_plan {
            let command_envelope = CommandEnvelope::from_parts(
                CommandId::new(),
                CommandSource::from_key_binding(client_id),
                *command,
            );
            let _ = self.dispatch(command_envelope);
        }
        self.render_scheduler.invalidate();
    }
}

fn build_test_server() -> (Server, Arc<FakePtyBackend>, ClientId, ViewerClient) {
    let (event_sender, inbox_receiver) = mpsc::channel();
    let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
        InboxSink::from_event_sender(event_sender),
    )));
    let mut server = Server::from_runtime_parts(fake_pty_backend.clone(), inbox_receiver);
    let client_id = server
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let viewer = build_viewer(&mut server, client_id);
    (server, fake_pty_backend, client_id, viewer)
}

/// One keypress, the way the running binary delivers it: the viewer decides
/// what the chord means, and only what it resolves to reaches the session.
fn apply_key_press(
    server: &mut Server,
    viewer: &mut ViewerClient,
    key_chord: KeyChord,
    current_time: Instant,
) {
    let client_id = viewer.get_client_id();
    match viewer.resolve_key(key_chord, current_time) {
        KeyOutcome::Fire(bound_action) => {
            let new_pane_direction = viewer.get_client_config().layout.new_pane_direction;
            server.handle_bound_action(client_id, bound_action, new_pane_direction);
        }
        KeyOutcome::PassThrough(passthrough_key_chord) => {
            server.handle_key_input(client_id, &build_key_input_for_chord(passthrough_key_chord));
        }
        KeyOutcome::Pending | KeyOutcome::Discard => {}
    }
    viewer.apply_events();
}

/// Fire an open sequence's binding if its ambiguity deadline has passed.
fn expire_pending_key_sequence(
    server: &mut Server,
    viewer: &mut ViewerClient,
    current_time: Instant,
) {
    if let Some(bound_action) = viewer.expire_key_sequence(current_time) {
        let new_pane_direction = viewer.get_client_config().layout.new_pane_direction;
        server.handle_bound_action(viewer.get_client_id(), bound_action, new_pane_direction);
    }
    viewer.apply_events();
}

fn build_key_chord(modifier_flags: BindingModifierFlags, key_character: char) -> KeyChord {
    KeyChord::from_parts(modifier_flags, Key::Char(key_character))
}

/// An unmodified named key, for the arrows the default focus and resize
/// sequences continue with.
fn build_named_key_chord(named_key: NamedKey) -> KeyChord {
    KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(named_key))
}

fn get_only_pane_id(server: &Server) -> koshi_core::ids::PaneId {
    *server.live_pane_ids.iter().next().expect("one pane")
}

/// Whether the session still holds `client_id`.
fn is_client_attached(server: &Server, client_id: ClientId) -> bool {
    server
        .list_sessions()
        .values()
        .next()
        .expect("one session")
        .clients
        .get_client_by_id(client_id)
        .is_some()
}

/// The client's scroll offset for the pane — `0` means the view follows live output.
fn get_client_scroll_offset(
    server: &Server,
    client_id: ClientId,
    pane_id: koshi_core::ids::PaneId,
) -> usize {
    server
        .list_sessions()
        .values()
        .next()
        .expect("one session")
        .clients
        .get_client_by_id(client_id)
        .expect("client present")
        .get_scroll_offset(pane_id)
}

/// A bootstrapped server whose one client has scrolled its view 3 lines up into
/// a pane's history.
fn build_server_with_scrolled_client() -> (Server, koshi_core::ids::PaneId, ClientId, ViewerClient)
{
    let (mut server, _fake_pty_backend, client_id, viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    server.handle_pty_output(pane_id, &b"\n".repeat(40)); // push lines into history
    server.scroll_up(client_id, pane_id, 3);
    (server, pane_id, client_id, viewer)
}

/// Runs `command` as if `client_id` had issued it from a keybinding. Panics if
/// the command is rejected.
fn dispatch_test_command(server: &mut Server, client_id: ClientId, command: Command) {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        command,
    );
    let dispatch_result = server.dispatch(command_envelope);
    assert!(
        matches!(dispatch_result, CommandResult::Ok { .. }),
        "test setup: command was rejected: {dispatch_result:?}"
    );
}

#[test]
fn unbound_plain_key_passes_to_focused_pty() {
    let (mut server, fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![b'a']]
    );
}

#[test]
fn an_unbound_arrow_follows_the_focused_panes_application_cursor_mode() {
    let (mut server, fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let up_key_chord = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Up));

    // A shell leaves application-cursor-keys mode off, and reads `ESC [ A`.
    apply_key_press(&mut server, &mut viewer, up_key_chord, Instant::now());
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![b"\x1b[A".to_vec()]
    );

    // vim turns it on (DECCKM, `ESC [ ? 1 h`) and now reads `ESC O A` for the
    // same press. The pane's mode, not the press, picks the bytes.
    server.handle_pty_output(pane_id, b"\x1b[?1h");
    apply_key_press(&mut server, &mut viewer, up_key_chord, Instant::now());
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![b"\x1b[A".to_vec(), b"\x1bOA".to_vec()]
    );
}

#[test]
fn a_buffered_key_reaches_no_pane_at_all_even_after_focus_moves() {
    // The chords of an unfinished key sequence belong to koshi, not to any
    // pane. Focus can move while one waits, for example on a `core:focus-pane`
    // command over IPC. No pane ever receives the buffered key.
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let original_pane_id = get_only_pane_id(&server);
    let up_key_chord = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Up));
    let current_time = Instant::now();

    // A second pane, which takes focus. It runs vim: application-cursor-keys on.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    let focused_pane_id = get_focused_pane_id(&server, client_id);
    assert_ne!(focused_pane_id, original_pane_id);
    server.handle_pty_output(focused_pane_id, b"\x1b[?1h");

    // `<Up> x` makes a bare `<Up>` a prefix. Pressing `<Up>` opens a sequence.
    configure_normal_keybinding(
        &mut viewer,
        KeySequence::from_first_and_rest(
            up_key_chord,
            vec![build_key_chord(BindingModifierFlags::NONE, 'x')],
        ),
        ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
    );
    apply_key_press(&mut server, &mut viewer, up_key_chord, current_time);

    // Focus moves off that pane without a keypress, on a `core:focus-pane`
    // command from the mouse. Only a keypress touches a pending sequence: the
    // buffered `<Up>` is still open when the focused pane changes.
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::Mouse { client_id },
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(original_pane_id),
            client_id: Some(client_id),
        }),
    );
    let dispatch_result = server.dispatch(command_envelope);
    assert_eq!(
        get_focused_pane_id(&server, client_id),
        original_pane_id,
        "{dispatch_result:?}"
    );

    // `z` continues nothing: it is discarded, and the sequence stands. Neither
    // pane sees a byte: not the buffered `<Up>`, not the `z`.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'z'),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(focused_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(original_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(up_key_chord)),
        "the open sequence outlives a key it cannot use"
    );

    // Escape leaves the sequence, and still nothing is typed at either pane.
    apply_key_press(
        &mut server,
        &mut viewer,
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(focused_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(original_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_buffered_arrow_is_never_written_even_when_its_pane_flips_cursor_mode() {
    // A pane can turn application-cursor-keys mode on from its own output while
    // a sequence waits. The buffered `<Up>` is never written in either mode.
    let (mut server, fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let up_key_chord = KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Up));

    // `<Up> x` makes a bare `<Up>` a prefix. Pressing `<Up>` opens a sequence
    // and passes nothing through.
    configure_normal_keybinding(
        &mut viewer,
        KeySequence::from_first_and_rest(
            up_key_chord,
            vec![build_key_chord(BindingModifierFlags::NONE, 'x')],
        ),
        ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
    );

    // Press `<Up>` while the pane is a plain shell: buffered, nothing written.
    let current_time = Instant::now();
    apply_key_press(&mut server, &mut viewer, up_key_chord, current_time);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );

    // The pane now turns application-cursor-keys mode ON, mid-sequence.
    server.handle_pty_output(pane_id, b"\x1b[?1h");

    // `z` continues nothing. It is discarded and the sequence stands. The pane
    // sees neither the arrow nor the `z`, in either cursor mode.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'z'),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );

    // Completing the sequence fires the binding and still writes no bytes to
    // the pane.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'x'),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_modified_arrow_keeps_its_modifier_on_the_way_to_the_pane() {
    let (mut server, fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);

    // `<C-Right>` reaches the pane as `ESC [ 1 ; 5 C`, with the Control
    // modifier kept.
    apply_key_press(
        &mut server,
        &mut viewer,
        KeyChord::from_parts(BindingModifierFlags::CTRL, Key::Named(NamedKey::Right)),
        Instant::now(),
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![b"\x1b[1;5C".to_vec()]
    );
}

#[test]
fn the_lock_chord_flips_the_client_both_ways_without_pty_bytes() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();
    // `<C-l>` locks in normal mode…
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'l'),
        current_time,
    );
    assert_eq!(
        server
            .get_session_for_client(client_id)
            .unwrap()
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_lock_mode(),
        LockMode::Locked
    );
    // …and the same chord is the reserved unlock in locked mode.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'l'),
        current_time,
    );
    assert_eq!(
        server
            .get_session_for_client(client_id)
            .unwrap()
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_lock_mode(),
        LockMode::Normal
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn quit_binding_detaches_the_client_in_normal_mode() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'q'),
        Instant::now(),
    );
    // `auto-close-session` defaults off. The client leaves and the session
    // keeps running.
    assert!(!is_client_attached(&server, client_id));
    assert!(!server.is_quit_requested());
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn quit_binding_detaches_the_client_in_locked_mode_too() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'l'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'q'),
        current_time,
    );
    assert!(!is_client_attached(&server, client_id));
    assert!(!server.is_quit_requested());
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn quit_binding_ends_the_session_when_auto_close_is_on() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    server.config.should_auto_close_session = true;
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'q'),
        Instant::now(),
    );
    // The sole client leaves and the setting ends the session. Teardown keeps
    // the graceful window.
    assert!(!is_client_attached(&server, client_id));
    assert!(server.is_quit_requested());
    assert!(!server.should_shutdown_immediately);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn continuous_resize_keeps_the_prefix_armed_for_repeat_presses() {
    let (mut server, _fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );

    // First resize: full `<C-s> <Left>` sequence.
    let pty_sizes_before = server.pty_size_by_pane_id.clone();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 's'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        current_time,
    );
    let pty_sizes_after_first_resize = server.pty_size_by_pane_id.clone();
    assert_ne!(pty_sizes_after_first_resize, pty_sizes_before);

    // The prefix stayed armed: `<Left>` alone fires the resize again…
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            's'
        )))
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        current_time,
    );
    assert_ne!(server.pty_size_by_pane_id, pty_sizes_after_first_resize);

    // …and Escape puts the bar back to idle.
    apply_key_press(
        &mut server,
        &mut viewer,
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
        current_time,
    );
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn one_shot_bindings_clear_the_whole_sequence_after_firing() {
    let (mut server, _fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    // `new-pane` is not continuous: after `<C-p> n` fires, nothing pends.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    assert_eq!(server.live_pane_ids.len(), 2);
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn locked_mode_passes_non_unlock_keys_verbatim() {
    let (mut server, fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'l'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'x'),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![b'x']]
    );
}

#[test]
fn pane_prefix_updates_snapshot_then_new_pane_fires() {
    let (mut server, _fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'p'
        )))
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    assert_eq!(server.live_pane_ids.len(), 2);
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn prefix_pending_never_expires() {
    let (mut server, fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    // A prefix-only sequence arms no deadline and outlives any wait: the
    // continuation hints stay up until the user presses another key.
    assert_eq!(viewer.compute_next_key_wakeup(current_time), None);
    expire_pending_key_sequence(
        &mut server,
        &mut viewer,
        current_time + Duration::from_secs(3600),
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'p'
        )))
    );
}

#[test]
fn escape_cancels_a_pending_sequence_silently() {
    let (mut server, fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
        current_time,
    );
    // Neither the buffered prefix nor the Escape reaches the pane, and the
    // pending sequence is gone: the bar returns to its idle hints.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn an_unmatched_continuation_is_discarded_and_the_sequence_stands() {
    let (mut server, fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();
    // `<C-p>` opens the pane prefix. `z` binds nothing under it: it goes
    // nowhere, and the prefix is still open. The shell sees neither `Ctrl-P`
    // nor the `z`.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'z'),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'p'
        )))
    );

    // The sequence is live, not merely remembered: `n` still completes it.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    assert_eq!(server.live_pane_ids.len(), 2);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn directional_focus_binding_moves_focus_across_a_split() {
    let (mut server, _fake_pty_backend, client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    // Split: the new right pane takes focus.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    let focused_pane_id_after_split = get_focused_pane_id(&server, client_id);

    // `<C-p> <Left>` focuses the left neighbor.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        current_time,
    );
    let focused_left_pane_id = get_focused_pane_id(&server, client_id);
    assert_ne!(focused_left_pane_id, focused_pane_id_after_split);

    // Focus is continuous: the prefix stays armed, and `<Right>` alone returns
    // to the right pane.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_named_key_chord(NamedKey::Right),
        current_time,
    );
    assert_eq!(
        get_focused_pane_id(&server, client_id),
        focused_pane_id_after_split
    );
}

#[test]
fn directional_new_pane_binding_splits_on_that_side() {
    let (mut server, _fake_pty_backend, client_id, mut viewer) = build_test_server();
    let original_pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();

    // `<C-p> h` opens a new pane on the left of the focused one, and the new
    // pane takes focus.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'h'),
        current_time,
    );
    assert_eq!(server.live_pane_ids.len(), 2);
    let new_pane_id = get_focused_pane_id(&server, client_id);
    assert_ne!(new_pane_id, original_pane_id);

    // The original pane is the new pane's right neighbor, where a left split
    // puts it.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_named_key_chord(NamedKey::Right),
        current_time,
    );
    assert_eq!(get_focused_pane_id(&server, client_id), original_pane_id);
}

#[test]
fn the_viewers_configured_split_direction_reaches_the_new_pane() {
    let (mut server, _fake_pty_backend, client_id, mut viewer) = build_test_server();
    let original_pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();

    // The viewer folds `layout.new-pane-direction "down"` out of its own
    // `koshi.kdl`. The fired binding carries that direction to the split.
    // Nothing else in the process holds a split direction.
    viewer.load_startup_config(
        Some(PartialKoshiConfig {
            layout: Some(PartialLayoutDefaults {
                new_pane_direction: Some(Direction::Down),
            }),
            ..PartialKoshiConfig::default()
        }),
        None,
        None,
    );
    assert_eq!(
        viewer.get_client_config().layout.new_pane_direction,
        Direction::Down
    );

    let tab_id = get_active_tab_id(&server, client_id);
    let original_layout_tree = server
        .get_session_for_client(client_id)
        .expect("session")
        .tabs[&tab_id]
        .get_layout_tree()
        .clone();

    // `<C-p> n` is the direction-less new-pane binding.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );

    let new_pane_id = get_focused_pane_id(&server, client_id);
    assert_ne!(new_pane_id, original_pane_id);
    let expected_layout_tree = split_leaf(
        &original_layout_tree,
        original_pane_id,
        new_pane_id,
        Direction::Down,
    )
    .expect("split on the source leaf");
    assert_eq!(
        server
            .get_session_for_client(client_id)
            .expect("session")
            .tabs[&tab_id]
            .get_layout_tree(),
        &expected_layout_tree
    );
}

#[test]
fn a_user_bound_stacked_new_pane_key_builds_a_stack() {
    let (mut server, _fake_pty_backend, client_id, mut viewer) = build_test_server();
    let original_pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();

    // `new-pane-stacked` ships with no default key. The test binds `<A-s>`.
    configure_normal_keybinding(
        &mut viewer,
        KeySequence::from(build_key_chord(BindingModifierFlags::ALT, 's')),
        ActionReference::from_core_action_name("new-pane-stacked").expect("valid name"),
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::ALT, 's'),
        current_time,
    );

    // The leaf becomes a two-member stack: the source collapses to a header
    // and the new pane is the expanded, focused member.
    assert_eq!(server.live_pane_ids.len(), 2);
    let new_pane_id = get_focused_pane_id(&server, client_id);
    assert_ne!(new_pane_id, original_pane_id);
    let session = server.get_session_for_client(client_id).expect("session");
    let tab_id = session
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab_id();
    assert_eq!(
        session.tabs[&tab_id].get_layout_tree(),
        &LayoutNode::Split(SplitNode::from_stacked_pane_ids(
            vec![original_pane_id, new_pane_id],
            1
        ))
    );
}

#[test]
fn fullscreen_binding_toggles_the_layout_mode() {
    let (mut server, _fake_pty_backend, client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::ALT, 'f'),
        current_time,
    );
    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .layout_mode,
        koshi_layout::mode::LayoutMode::Fullscreen {
            focused_pane_id: get_focused_pane_id(&server, client_id)
        }
    );

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::ALT, 'f'),
        current_time,
    );
    let render_snapshot = server.build_snapshot(client_id).expect("snapshot");
    assert_eq!(
        render_snapshot
            .session_snapshot
            .active_tab_snapshot
            .layout_mode,
        koshi_layout::mode::LayoutMode::Tiled
    );
}

fn get_focused_pane_id(server: &Server, client_id: ClientId) -> koshi_core::ids::PaneId {
    let session = server.get_session_for_client(client_id).expect("session");
    let client_state = session.clients.get_client_by_id(client_id).expect("client");
    client_state
        .get_focused_pane_id(client_state.get_active_tab_id())
        .expect("a focused pane")
}

/// The tab the client is looking at.
fn get_active_tab_id(server: &Server, client_id: ClientId) -> koshi_core::ids::TabId {
    server
        .get_session_for_client(client_id)
        .expect("session")
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab_id()
}

/// Attaches a second local client to `first_client_id`'s session, on the
/// first client's active tab with `focused_pane_id` focused, and returns the
/// second client's id.
fn attach_second_client(
    server: &mut Server,
    first_client_id: ClientId,
    focused_pane_id: koshi_core::ids::PaneId,
) -> ClientId {
    let session_id = server
        .get_session_for_client(first_client_id)
        .expect("session")
        .session_id;
    let tab_id = get_active_tab_id(server, first_client_id);
    let second_client_id = ClientId::new();
    let mut second_client = Client::from_attachment(
        second_client_id,
        session_id,
        SystemTime::now(),
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        tab_id,
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    second_client.update_focused_pane(tab_id, focused_pane_id);
    server
        .session_by_id
        .get_mut(&session_id)
        .expect("session")
        .attach_client(second_client);
    second_client_id
}

#[test]
fn resize_prefix_moves_a_live_split_border() {
    let (mut server, _fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    let pty_sizes_before = server.pty_size_by_pane_id.clone();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 's'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        current_time,
    );
    assert_ne!(server.pty_size_by_pane_id, pty_sizes_before);
}

#[test]
fn continuous_focus_rearm_walks_panes_with_repeated_arrows() {
    let (mut server, _fake_pty_backend, client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    // Two splits: three panes across, focus on the right-most.
    for _ in 0..2 {
        apply_key_press(
            &mut server,
            &mut viewer,
            build_key_chord(BindingModifierFlags::CTRL, 'p'),
            current_time,
        );
        apply_key_press(
            &mut server,
            &mut viewer,
            build_key_chord(BindingModifierFlags::NONE, 'n'),
            current_time,
        );
    }
    let rightmost_pane_id = get_focused_pane_id(&server, client_id);

    // `<C-p> ←` moves one pane left and re-arms the prefix…
    let left_key_chord =
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Left));
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(&mut server, &mut viewer, left_key_chord, current_time);
    let middle_pane_id = get_focused_pane_id(&server, client_id);
    assert_ne!(middle_pane_id, rightmost_pane_id);
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'p'
        )))
    );

    // …and a bare ← walks one further pane left.
    apply_key_press(&mut server, &mut viewer, left_key_chord, current_time);
    let leftmost_pane_id = get_focused_pane_id(&server, client_id);
    assert_ne!(leftmost_pane_id, middle_pane_id);
    assert_ne!(leftmost_pane_id, rightmost_pane_id);
}

#[test]
fn abandoned_rearmed_prefix_writes_nothing_to_the_pane() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    let focused_pane_id = get_focused_pane_id(&server, client_id);

    // Resize once, leave the re-armed prefix hanging, then cancel with Esc.
    // The re-armed prefix carries no fallback bytes, and the shell sees none.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 's'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(focused_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn an_unmatched_key_under_a_rearmed_prefix_is_discarded_and_it_stays_armed() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    let focused_pane_id = get_focused_pane_id(&server, client_id);

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 's'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        current_time,
    );
    let pty_sizes_after_one_resize = server.pty_size_by_pane_id.clone();

    // A re-armed prefix holds keys like any other open sequence. `z` resizes
    // nothing: it is discarded, not passed to the shell, and `<C-s>` stays
    // armed.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'z'),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(focused_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(server.pty_size_by_pane_id, pty_sizes_after_one_resize);
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            's'
        )))
    );

    // Still armed: the next `<Left>` resizes again without re-pressing `<C-s>`.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        current_time,
    );
    assert_ne!(server.pty_size_by_pane_id, pty_sizes_after_one_resize);

    // Escape is the way out, and it types nothing at the pane.
    apply_key_press(
        &mut server,
        &mut viewer,
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(focused_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn resize_binding_at_the_tab_edge_moves_the_opposite_border() {
    let (mut server, _fake_pty_backend, client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    let focused_pane_id = get_focused_pane_id(&server, client_id);
    let original_pty_size = server.pty_size_by_pane_id[&focused_pane_id];

    // The focused pane touches the tab's right edge. `<C-s> <Right>` has no
    // right border to grow through: the left border moves right and the pane
    // shrinks by one column.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 's'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_named_key_chord(NamedKey::Right),
        current_time,
    );
    let resized_pty_size = server.pty_size_by_pane_id[&focused_pane_id];
    assert_eq!(
        resized_pty_size.column_count,
        original_pty_size.column_count - 1
    );
    assert_eq!(resized_pty_size.row_count, original_pty_size.row_count);
}

/// Bind one `normal`-mode sequence to `action_reference` in the viewer's own keymap.
fn configure_normal_keybinding(
    viewer: &mut ViewerClient,
    key_sequence: KeySequence,
    action_reference: ActionReference,
) {
    configure_normal_keybindings(viewer, vec![(key_sequence, action_reference)]);
}

/// Bind one `locked`-mode sequence to `action_reference` beside the shipped
/// locked bindings, the unlock chord among them.
fn configure_locked_keybinding(
    viewer: &mut ViewerClient,
    key_sequence: KeySequence,
    action_reference: ActionReference,
) {
    let mut bound_action_by_key_sequence = KeybindingsConfig::default()
        .mode_bindings_by_name
        .remove(&ModeName::from_text("locked"))
        .expect("the shipped config binds locked mode")
        .bound_action_by_key_sequence;
    bound_action_by_key_sequence.insert(key_sequence, BoundAction { action_reference });
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("locked"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: BTreeSet::new(),
        },
    );
    let keymap_report = viewer.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
    );
    assert_eq!(
        keymap_report
            .expect("a keybinding file was given")
            .get_verdict(),
        KeymapVerdict::Apply,
        "test setup: the candidate binding must apply cleanly"
    );
}

/// How long the viewer waits for the next chord of an ambiguous sequence.
fn get_chord_timeout(viewer: &ViewerClient) -> Duration {
    Duration::from_millis(u64::from(
        viewer.get_client_config().keybindings.chord_timeout_ms,
    ))
}

/// How many tabs the server's one session holds.
fn count_session_tabs(server: &Server) -> usize {
    server
        .list_sessions()
        .values()
        .next()
        .expect("one session")
        .tabs
        .len()
}

/// Binds `<C-y>` to `new-tab` and `<C-y> x` to `unlock` in normal mode.
/// `<C-y>` alone is then both a complete binding and a prefix, and pressing it
/// opens a sequence with an ambiguity deadline.
fn configure_ambiguous_ctrl_y_keybindings(viewer: &mut ViewerClient) {
    configure_normal_keybindings(
        viewer,
        vec![
            (
                KeySequence::from(build_key_chord(BindingModifierFlags::CTRL, 'y')),
                ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
            ),
            (
                KeySequence::from_first_and_rest(
                    build_key_chord(BindingModifierFlags::CTRL, 'y'),
                    vec![build_key_chord(BindingModifierFlags::NONE, 'x')],
                ),
                ActionReference::from_core_action_name("unlock").expect("valid core action name"),
            ),
        ],
    );
}

/// The client's current lock mode.
fn get_client_lock_mode(server: &Server, client_id: ClientId) -> LockMode {
    server
        .get_session_for_client(client_id)
        .expect("session")
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_lock_mode()
}

/// Bind every `(key_sequence, action_reference)` pair in `bindings` under
/// `normal` mode in one `keybinding.kdl` the viewer reads. Each call replaces
/// the whole keybinding layer that an earlier call set.
fn configure_normal_keybindings(
    viewer: &mut ViewerClient,
    bindings: Vec<(KeySequence, ActionReference)>,
) {
    let mut bound_action_by_key_sequence = BTreeMap::new();
    for (key_sequence, action_reference) in bindings {
        bound_action_by_key_sequence.insert(key_sequence, BoundAction { action_reference });
    }
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences: BTreeSet::new(),
        },
    );
    let keymap_report = viewer.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            mode_bindings_by_name: Some(mode_bindings_by_name),
            ..PartialKeybindingsConfig::default()
        }),
    );
    assert_eq!(
        keymap_report
            .expect("a keybinding file was given")
            .get_verdict(),
        KeymapVerdict::Apply,
        "test setup: the candidate binding must apply cleanly"
    );
}

#[test]
fn a_key_from_an_unknown_client_writes_nothing() {
    let (mut server, fake_pty_backend, _client_id, _viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);

    // A press arriving for a client the session does not know resolves to no
    // pane and writes nothing.
    server.handle_key_input(
        ClientId::new(),
        &build_key_input_for_chord(build_key_chord(BindingModifierFlags::NONE, 'x')),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_key_writes_nothing_when_the_client_has_no_focused_pane() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let tab_id = server
        .get_session_for_client(client_id)
        .expect("session")
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab_id();
    server
        .get_session_for_client_mut(client_id)
        .expect("session")
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .remove_focused_pane(tab_id);

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'x'),
        Instant::now(),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

/// A tab whose only viewer reports [`PaneArea::Starving`] has no effective
/// size: it solves to no drawn pane and takes no keystroke.
#[test]
fn a_key_from_a_starving_sole_viewer_writes_nothing() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    server
        .get_session_for_client_mut(client_id)
        .expect("session")
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .update_pane_area(Some(PaneArea::Starving));

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'x'),
        Instant::now(),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

/// A pane the tab has no room to draw takes no keystroke. The terminal shrinks
/// below the pane's minimum, the pane is suppressed, and `l` reaches no shell.
#[test]
fn a_key_writes_nothing_when_the_focused_pane_is_suppressed() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);

    // Shrink the terminal until the sole pane no longer fits: a pane needs
    // MIN_PANE_SIZE plus its one-cell border, and 3x3 leaves less than that.
    server.handle_client_resize(
        client_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
        None,
        None,
    );
    assert!(
        server
            .build_snapshot(client_id)
            .expect("snapshot")
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed,
        "test setup: the sole pane must be suppressed at this size"
    );

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'l'),
        Instant::now(),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

/// A key still reaches the pane once the terminal grows back: suppression
/// blocks the write while it lasts, and leaves nothing latched behind it.
#[test]
fn a_key_reaches_the_pane_again_once_it_is_no_longer_suppressed() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);

    server.handle_client_resize(
        client_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
        None,
        None,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'l'),
        Instant::now(),
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );

    server.handle_client_resize(
        client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
        None,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'l'),
        Instant::now(),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![b'l']]
    );
}

/// Zoom is per-client: one client zooming a pane does not silence another
/// client's keys. Client A zooms its pane. Client B, tiled on the same tab,
/// keeps typing into the pane B can still see. The key guard asks whether the
/// layout draws the pane for the typing client.
#[test]
fn one_client_zooming_does_not_stop_another_client_keys() {
    let (mut server, fake_pty_backend, first_client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    let original_pane_id = get_only_pane_id(&server);

    // Split so the tab has two panes; client A's focus lands on the new one.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    let focused_pane_id = get_focused_pane_id(&server, first_client_id);
    assert_ne!(focused_pane_id, original_pane_id);

    // Client B joins the tab, focused on the first pane.
    let second_client_id = attach_second_client(&mut server, first_client_id, original_pane_id);
    let mut second_viewer = build_viewer(&mut server, second_client_id);

    // Client A zooms its own pane and hides `original_pane_id` from A's view only.
    dispatch_test_command(&mut server, first_client_id, Command::TogglePaneFullscreen);

    // B types. B is tiled and sees `original_pane_id`, and its key lands there.
    apply_key_press(
        &mut server,
        &mut second_viewer,
        build_key_chord(BindingModifierFlags::NONE, 'y'),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(original_pane_id)
            .expect("writes"),
        vec![vec![b'y']]
    );

    // A types. A sees its zoomed pane, and its key lands there.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'z'),
        current_time,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(focused_pane_id)
            .expect("writes"),
        vec![vec![b'z']]
    );
}

/// Two clients view one tab holding a stack. Only the stack's active member is
/// drawn; the others collapse to a one-line header. Focus is per-client, and
/// the active member belongs to the tab. Client B activating its member
/// collapses the pane client A still has focused, and client A's keys stop
/// reaching it. The collapsed pane is not suppressed: it draws no content.
#[test]
fn a_key_writes_nothing_when_the_focused_pane_collapsed_to_a_stack_header() {
    let (mut server, fake_pty_backend, first_client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    let original_pane_id = get_only_pane_id(&server);

    // Stack a second pane onto the original pane. The new member becomes the
    // active one and takes client A's focus; the original pane collapses to a header.
    dispatch_test_command(
        &mut server,
        first_client_id,
        Command::NewPane(NewPaneArgs {
            placement: NewPanePlacement::Stacked {
                source_pane_id: Some(original_pane_id),
                tab_id: None,
            },
            working_directory: None,
            spawn_spec: None,
            client_id: Some(first_client_id),
        }),
    );
    let stacked_pane_id = get_focused_pane_id(&server, first_client_id);
    assert_ne!(
        stacked_pane_id, original_pane_id,
        "test setup: the stacked pane took focus"
    );

    // Client B joins the same tab.
    let second_client_id = attach_second_client(&mut server, first_client_id, stacked_pane_id);

    // Client B focuses the other member and activates it. The stacked pane
    // client A still has focused collapses to a header.
    dispatch_test_command(
        &mut server,
        second_client_id,
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(original_pane_id),
            client_id: Some(second_client_id),
        }),
    );
    assert_eq!(
        get_focused_pane_id(&server, first_client_id),
        stacked_pane_id,
        "test setup: the first client's focus did not move"
    );

    // Client A types at a pane that draws nothing. Client B types at the member
    // that is drawn.
    let mut second_viewer = build_viewer(&mut server, second_client_id);
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'z'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut second_viewer,
        build_key_chord(BindingModifierFlags::NONE, 'y'),
        current_time,
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(stacked_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(original_pane_id)
            .expect("writes"),
        vec![vec![b'y']]
    );
}

#[test]
fn pending_sequences_stay_independent_across_clients_in_the_same_session() {
    let (mut server, fake_pty_backend, first_client_id, mut viewer) = build_test_server();
    let current_time = Instant::now();
    let original_pane_id = get_only_pane_id(&server);

    // Split: client A's focus moves to the new pane, and no client focuses
    // `original_pane_id`.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'n'),
        current_time,
    );
    let first_client_pane_id = get_focused_pane_id(&server, first_client_id);
    assert_ne!(first_client_pane_id, original_pane_id);

    // Client B joins the same session, focused on the original pane, a
    // different pane than client A's.
    let second_client_id = attach_second_client(&mut server, first_client_id, original_pane_id);

    // Each viewer holds its own keymap and its own open sequence.
    let mut second_viewer = build_viewer(&mut server, second_client_id);

    // Client A opens the pane prefix and leaves it hanging...
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        current_time,
    );
    // ...client B, meanwhile, sends an unrelated unbound key straight through
    // on its own (different) pane.
    apply_key_press(
        &mut server,
        &mut second_viewer,
        build_key_chord(BindingModifierFlags::NONE, 'z'),
        current_time,
    );

    // Only `z` reaches client B's own pane, never client A's buffered `<C-p>`
    // byte. Client A's pane sees nothing at all.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(original_pane_id)
            .expect("writes"),
        vec![vec![b'z']]
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(first_client_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'p'
        ))),
        "client A's sequence is still open"
    );
    assert_eq!(
        second_viewer.get_pending_key_sequence().cloned(),
        None,
        "client B never opened one of its own"
    );
}

#[test]
fn one_viewer_open_sequence_is_invisible_to_another_viewer() {
    // Each viewer owns the sequence it is typing. A prefix one viewer holds
    // open is never continued by another viewer's next key.
    let (mut server, _fake_pty_backend, first_client_id, mut viewer) = build_test_server();
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'p'),
        Instant::now(),
    );

    // Client B joins the same session with no sequence of its own.
    let only_pane_id = get_only_pane_id(&server);
    let second_client_id = attach_second_client(&mut server, first_client_id, only_pane_id);
    let mut second_viewer = build_viewer(&mut server, second_client_id);

    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'p'
        ))),
        "client A is mid-sequence"
    );
    assert_eq!(
        second_viewer.get_pending_key_sequence().cloned(),
        None,
        "client B has nothing open"
    );

    // B's `n` is its own key, not the continuation of `<C-p> n`. It passes
    // through, and A's sequence is still waiting.
    assert_eq!(
        second_viewer.resolve_key(
            build_key_chord(BindingModifierFlags::NONE, 'n'),
            Instant::now()
        ),
        KeyOutcome::PassThrough(build_key_chord(BindingModifierFlags::NONE, 'n'))
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'p'
        ))),
        "A's sequence outlives B's keypress"
    );
}

#[test]
fn a_sequence_grows_to_the_chord_depth_cap_and_no_further() {
    let (mut server, fake_pty_backend, _client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    // A 4-chord binding, exactly the default `maximum_chord_depth`. A sequence
    // only grows while a longer live binding still starts with it, and the
    // merge drops any binding past the cap.
    let long_key_sequence = KeySequence::from_first_and_rest(
        build_key_chord(BindingModifierFlags::CTRL, 'y'),
        vec![
            build_key_chord(BindingModifierFlags::NONE, 'a'),
            build_key_chord(BindingModifierFlags::NONE, 'b'),
            build_key_chord(BindingModifierFlags::NONE, 'c'),
        ],
    );
    configure_normal_keybinding(
        &mut viewer,
        long_key_sequence.clone(),
        ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
    );
    let tab_count_before = count_session_tabs(&server);

    let current_time = Instant::now();
    for key_chord in long_key_sequence.list_chords() {
        apply_key_press(&mut server, &mut viewer, *key_chord, current_time);
    }

    // The full-depth binding fires, the sequence closes, and nothing along the
    // way was typed at the pane.
    assert_eq!(count_session_tabs(&server), tab_count_before + 1);
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn the_unlock_chord_escapes_a_locked_client_from_inside_an_open_sequence() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();
    // A locked-mode sequence of the user's own: `<C-x> a`. Pressing `<C-x>`
    // opens it, and the client is locked and mid-sequence at once.
    configure_locked_keybinding(
        &mut viewer,
        KeySequence::from_first_and_rest(
            build_key_chord(BindingModifierFlags::CTRL, 'x'),
            vec![build_key_chord(BindingModifierFlags::NONE, 'a')],
        ),
        ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'l'),
        current_time,
    );
    assert_eq!(get_client_lock_mode(&server, client_id), LockMode::Locked);
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'x'),
        current_time,
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'x'
        )))
    );

    // The unlock chord resolves ahead of the keymap and ahead of the open
    // sequence: the client unlocks, the held `<C-x>` is dropped and not typed
    // at the pane, and no pending sequence survives into normal mode.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'l'),
        current_time,
    );
    assert_eq!(get_client_lock_mode(&server, client_id), LockMode::Normal);
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_locked_binding_holding_the_unlock_chord_never_fires_and_never_captures() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let current_time = Instant::now();
    // `<C-x> <C-l>` in locked mode: the unlock resolves at the `<C-l>` wherever
    // it is pressed, and this binding never fires. The config merge drops it as
    // dead, and `<C-x>` does not become a prefix.
    configure_locked_keybinding(
        &mut viewer,
        KeySequence::from_first_and_rest(
            build_key_chord(BindingModifierFlags::CTRL, 'x'),
            vec![KeybindingsConfig::RESERVED_UNLOCK],
        ),
        ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
    );
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'l'),
        current_time,
    );
    assert_eq!(get_client_lock_mode(&server, client_id), LockMode::Locked);
    let tab_count_before = server
        .list_sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    // The dead binding wins no key: `<C-x>` opens no sequence and passes to the
    // pane verbatim, the same as every unbound key in locked mode.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'x'),
        current_time,
    );
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![0x18]]
    );

    // The unlock still unlocks: it is a continuation of nothing.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'l'),
        current_time,
    );
    assert_eq!(get_client_lock_mode(&server, client_id), LockMode::Normal);
    assert_eq!(
        server
            .list_sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tab_count_before,
        "the dead binding's action never runs"
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![0x18]]
    );
}

#[test]
fn expire_key_sequences_before_the_deadline_leaves_pending_intact() {
    let (mut server, _fake_pty_backend, _client_id, mut viewer) = build_test_server();
    configure_ambiguous_ctrl_y_keybindings(&mut viewer);
    let current_time = Instant::now();
    let tab_count_before = count_session_tabs(&server);

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'y'),
        current_time,
    );
    let ambiguity_deadline = current_time + get_chord_timeout(&viewer);
    expire_pending_key_sequence(
        &mut server,
        &mut viewer,
        ambiguity_deadline - Duration::from_millis(1),
    );

    assert_eq!(count_session_tabs(&server), tab_count_before);
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'y'
        )))
    );
}

#[test]
fn expire_key_sequences_at_the_deadline_fires_the_ambiguous_bindings_exact_match() {
    let (mut server, _fake_pty_backend, _client_id, mut viewer) = build_test_server();
    configure_ambiguous_ctrl_y_keybindings(&mut viewer);
    let current_time = Instant::now();
    let tab_count_before = count_session_tabs(&server);

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'y'),
        current_time,
    );
    let ambiguity_deadline = current_time + get_chord_timeout(&viewer);
    expire_pending_key_sequence(&mut server, &mut viewer, ambiguity_deadline);

    assert_eq!(count_session_tabs(&server), tab_count_before + 1);
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn a_held_exact_binding_survives_a_key_it_cannot_use_and_fires_at_its_deadline() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    configure_ambiguous_ctrl_y_keybindings(&mut viewer);
    let current_time = Instant::now();
    let tab_count_before = count_session_tabs(&server);

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'y'),
        current_time,
    );
    // `z` extends `<C-y>` into nothing. It is discarded, the sequence stays
    // open, and its deadline still stands.
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'z'),
        current_time,
    );
    assert_eq!(
        count_session_tabs(&server),
        tab_count_before,
        "the held binding waits for its ambiguity deadline, not for a mismatch"
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(
            BindingModifierFlags::CTRL,
            'y'
        )))
    );

    // The deadline decides: `<C-y>`'s own binding fires, and the client lands on
    // the new tab. Neither the held chord nor the discarded `z` was ever typed.
    let ambiguity_deadline = current_time + get_chord_timeout(&viewer);
    expire_pending_key_sequence(&mut server, &mut viewer, ambiguity_deadline);
    assert_eq!(count_session_tabs(&server), tab_count_before + 1);
    let new_pane_id = get_focused_pane_id(&server, client_id);
    assert_ne!(new_pane_id, pane_id, "new-tab switched focus to a new pane");
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(new_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn typing_snaps_a_scrolled_up_view_back_to_live_output() {
    let (mut server, pane_id, client_id, mut viewer) = build_server_with_scrolled_client();
    assert_eq!(get_client_scroll_offset(&server, client_id, pane_id), 3);

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(get_client_scroll_offset(&server, client_id, pane_id), 0);
}

#[test]
fn typing_leaves_a_scrolled_up_view_in_place_when_scroll_on_input_is_off() {
    let (mut server, pane_id, client_id, mut viewer) = build_server_with_scrolled_client();
    server.client_config.scrollback.should_scroll_to_input = false;

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(get_client_scroll_offset(&server, client_id, pane_id), 3);
}

#[test]
fn typing_on_the_alternate_screen_leaves_the_view_to_the_program() {
    let (mut server, pane_id, client_id, mut viewer) = build_server_with_scrolled_client();
    server.handle_pty_output(pane_id, b"\x1b[?1049h"); // enter the alternate screen

    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(get_client_scroll_offset(&server, client_id, pane_id), 3);
}

#[test]
fn pasting_snaps_a_scrolled_up_view_back_to_live_output() {
    let (mut server, pane_id, client_id, _viewer) = build_server_with_scrolled_client();

    server.handle_host_paste(client_id, "ls\n");
    assert_eq!(get_client_scroll_offset(&server, client_id, pane_id), 0);
}

#[test]
fn an_empty_host_paste_writes_nothing() {
    let (mut server, fake_pty_backend, client_id, _viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);

    // Selecting nothing and hitting the OS paste key hands the session an
    // empty string. No empty write reaches the shell.
    server.handle_host_paste(client_id, "");

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_host_paste_from_an_unknown_client_writes_nothing() {
    let (mut server, fake_pty_backend, _client_id, _viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);

    // A paste arriving for a client the session does not know has no lock mode
    // to read and writes nothing.
    server.handle_host_paste(ClientId::new(), "ls");

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_host_paste_still_reaches_the_pane_while_the_client_is_locked() {
    let (mut server, fake_pty_backend, client_id, mut viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    apply_key_press(
        &mut server,
        &mut viewer,
        build_key_chord(BindingModifierFlags::CTRL, 'l'),
        Instant::now(),
    );
    assert_eq!(get_client_lock_mode(&server, client_id), LockMode::Locked);

    server.handle_host_paste(client_id, "ls");

    // Locked mode passes what it does not bind, and it binds no paste.
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![b"ls".to_vec()]
    );
}

#[test]
fn a_host_paste_writes_nothing_when_the_focused_pane_is_suppressed() {
    let (mut server, fake_pty_backend, client_id, _viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);

    // 3x3 leaves less than MIN_PANE_SIZE plus the pane's one-cell border. The
    // sole pane draws no content, and a paste reaches no pane.
    server.handle_client_resize(
        client_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
        None,
        None,
    );
    assert!(
        server
            .build_snapshot(client_id)
            .expect("snapshot")
            .session_snapshot
            .active_tab_snapshot
            .is_every_pane_suppressed,
        "test setup: the sole pane must be suppressed at this size"
    );

    server.handle_host_paste(client_id, "ls");

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_bound_action_the_session_does_not_know_dispatches_nothing() {
    let (mut server, fake_pty_backend, client_id, _viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let tab_count_before = count_session_tabs(&server);

    // A viewer keymap naming an action this session's registry has no entry
    // for: the action resolves to no plan, no command is dispatched, and the
    // chord is not written to the pane either.
    server.handle_bound_action(
        client_id,
        BoundAction {
            action_reference: ActionReference::from_core_action_name("no-such-action")
                .expect("valid core action name"),
        },
        Direction::Right,
    );

    assert_eq!(count_session_tabs(&server), tab_count_before);
    assert_eq!(server.live_pane_ids.len(), 1);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

// ------------------------ the pane's own keyboard flags decide the bytes ----

/// The bytes the fake backend saw written to `pane_id`, joined in order.
fn get_pane_write_bytes(
    fake_pty_backend: &FakePtyBackend,
    pane_id: koshi_core::ids::PaneId,
) -> Vec<u8> {
    fake_pty_backend
        .list_pane_write_bytes(pane_id)
        .expect("writes")
        .concat()
}

/// A Shift+Enter press, as a terminal that speaks the Kitty keyboard protocol
/// reports it.
fn build_shift_enter_press() -> KeyInput {
    KeyInput {
        key: KeyIdentity::Key(Key::Named(NamedKey::Enter)),
        key_event_kind: KeyEventKind::Press,
        shifted_key: None,
        base_layout_key: None,
        associated_text: String::new(),
        modifier_flags: KeyModifierFlags::SHIFT,
    }
}

#[test]
fn default_sends_legacy_shift_enter_to_pane_without_keyboard_flags() {
    let (mut server, fake_pty_backend, client_id, _viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);

    server.handle_key_input(client_id, &build_shift_enter_press());

    assert_eq!(
        get_pane_write_bytes(&fake_pty_backend, pane_id),
        b"\r".to_vec()
    );
}

#[test]
fn a_pane_that_pushed_flag_eight_reads_shift_enter_as_a_csi_u_report() {
    let (mut server, fake_pty_backend, client_id, _viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);

    // The program in the pane asks for every key as an escape code.
    server.handle_pty_output(pane_id, b"\x1b[>8u");
    server.handle_key_input(client_id, &build_shift_enter_press());

    assert_eq!(
        get_pane_write_bytes(&fake_pty_backend, pane_id),
        b"\x1b[13;2u".to_vec()
    );
}

#[test]
fn a_pane_reads_the_flags_of_the_screen_it_is_on_at_the_write() {
    let (mut server, fake_pty_backend, client_id, _viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);

    // The program pushes flag 8 on the primary screen, then enters the
    // alternate screen, whose own stack is empty.
    server.handle_pty_output(pane_id, b"\x1b[>8u\x1b[?1049h");
    server.handle_key_input(client_id, &build_shift_enter_press());
    assert_eq!(
        get_pane_write_bytes(&fake_pty_backend, pane_id),
        b"\r".to_vec()
    );

    // Back on the primary screen its own stack still holds flag 8.
    server.handle_pty_output(pane_id, b"\x1b[?1049l");
    server.handle_key_input(client_id, &build_shift_enter_press());
    assert_eq!(
        get_pane_write_bytes(&fake_pty_backend, pane_id),
        b"\r\x1b[13;2u".to_vec()
    );
}

#[test]
fn always_sends_distinct_shift_enter_to_pane_without_keyboard_flags() {
    let (mut server, fake_pty_backend, client_id, _viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    server.config.terminal.extended_keys_mode = ExtendedKeysMode::Always;

    server.handle_key_input(client_id, &build_shift_enter_press());

    assert_eq!(
        get_pane_write_bytes(&fake_pty_backend, pane_id),
        b"\x1b[13;2u".to_vec()
    );
}

#[test]
fn a_release_reaches_a_pane_only_when_that_pane_asked_for_event_kinds() {
    let (mut server, fake_pty_backend, client_id, _viewer) = build_test_server();
    let pane_id = get_only_pane_id(&server);
    let mut a_key_release_input =
        build_key_input_for_chord(build_key_chord(BindingModifierFlags::NONE, 'a'));
    a_key_release_input.key_event_kind = KeyEventKind::Release;

    server.handle_key_input(client_id, &a_key_release_input);
    assert_eq!(
        get_pane_write_bytes(&fake_pty_backend, pane_id),
        Vec::<u8>::new()
    );

    server.handle_pty_output(pane_id, b"\x1b[>10u");
    server.handle_key_input(client_id, &a_key_release_input);
    assert_eq!(
        get_pane_write_bytes(&fake_pty_backend, pane_id),
        b"\x1b[97;1:3u".to_vec()
    );
}
