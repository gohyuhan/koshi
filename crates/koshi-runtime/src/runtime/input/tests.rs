//! End-to-end input tests through the viewer's keymap: passthrough, lock
//! escape, open-sequence capture, multi-chord dispatch, timeout fallback,
//! which pane a press may reach, host paste, and pane resize.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc};

use koshi_config::conflict::KeymapVerdict;
use koshi_config::layer::{PartialKeybindingsConfig, PartialKoshiConfig, PartialLayoutDefaults};
use koshi_config::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::action::ActionReference;
use koshi_core::command::{Command, CommandResult, FocusPaneArgs, FocusTarget, NewPaneArgs};
use koshi_core::geometry::{Direction, PaneArea, Size};
use koshi_core::ids::{PluginId, SessionId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey};
use koshi_core::lock::LockMode;
use koshi_core::resolve::ActionArgs;
use koshi_layout::edit::split_leaf;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_pane::pane::state::PaneRecord;
use koshi_session::client::{Client, ClientOrigin};
use koshi_test_support::fake_pty::FakePtyBackend;
use std::time::{Duration, Instant};

use koshi_client::input::KeyOutcome;
use koshi_client::Client as ViewerClient;
use koshi_observability::cleanup::TerminalCleanupGuard;

use crate::runtime::bus::EventFilter;
use crate::server::Server;

fn build_test_runtime() -> (Server, Arc<FakePtyBackend>, ClientId, ViewerClient) {
    let fake_pty_backend = Arc::new(FakePtyBackend::new());
    let (event_sender, inbox_receiver) = mpsc::channel();
    let mut runtime =
        Server::from_runtime_parts(fake_pty_backend.clone(), inbox_receiver, event_sender);
    let client_id = runtime
        .bootstrap_local(
            SessionId::new(),
            Size {
                column_count: 80,
                row_count: 24,
            },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let viewer = build_viewer_client(&mut runtime, client_id);
    (runtime, fake_pty_backend, client_id, viewer)
}

/// The viewer half for `client_id`: it holds the keymap and resolves every
/// press below before the session hears about it.
fn build_viewer_client(runtime: &mut Server, client_id: ClientId) -> ViewerClient {
    ViewerClient::from_client_id_and_viewport(
        client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        runtime.subscribe(client_id, EventFilter::All),
        TerminalCleanupGuard::new(),
    )
}

/// One keypress, the way the running binary delivers it: the viewer decides
/// what the chord means, and only what it resolves to reaches the session.
fn apply_key_press(
    runtime: &mut Server,
    viewer: &mut ViewerClient,
    chord: KeyChord,
    current_time: Instant,
) {
    let client_id = viewer.get_client_id();
    match viewer.resolve_key(chord, current_time) {
        KeyOutcome::Fire(bound_action) => {
            let new_pane_direction = viewer.get_client_config().layout.new_pane_direction;
            runtime.handle_bound_action(client_id, bound_action, new_pane_direction);
        }
        KeyOutcome::PassThrough(chord) => runtime.handle_key_press(client_id, chord),
        KeyOutcome::Pending | KeyOutcome::Discard => {}
    }
    viewer.apply_events();
}

/// Fire an open sequence's binding if its ambiguity deadline has passed.
fn expire_pending_key_sequence(
    runtime: &mut Server,
    viewer: &mut ViewerClient,
    current_time: Instant,
) {
    if let Some(bound_action) = viewer.expire_key_sequence(current_time) {
        let new_pane_direction = viewer.get_client_config().layout.new_pane_direction;
        runtime.handle_bound_action(viewer.get_client_id(), bound_action, new_pane_direction);
    }
    viewer.apply_events();
}

fn build_key_chord(modifier_flags: ModFlags, key_character: char) -> KeyChord {
    KeyChord::from_parts(modifier_flags, Key::Char(key_character))
}

/// An unmodified named key, for the arrows the default focus and resize
/// sequences continue with.
fn build_named_key_chord(key: NamedKey) -> KeyChord {
    KeyChord::from_parts(ModFlags::NONE, Key::Named(key))
}

fn get_only_pane_id(runtime: &Server) -> koshi_core::ids::PaneId {
    *runtime
        .pty_handle_by_pane_id
        .keys()
        .next()
        .expect("one pane")
}

/// Whether the session still holds `client_id`.
fn is_client_attached(runtime: &Server, client_id: ClientId) -> bool {
    runtime
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
    runtime: &Server,
    client_id: ClientId,
    pane_id: koshi_core::ids::PaneId,
) -> usize {
    runtime
        .list_sessions()
        .values()
        .next()
        .expect("one session")
        .clients
        .get_client_by_id(client_id)
        .expect("client present")
        .get_scroll_offset(pane_id)
}

/// A bootstrapped runtime whose one client has scrolled its view 3 lines up into a
/// pane's history — the parked-view starting point the `scroll-on-input` tests share.
fn build_runtime_with_scrolled_client() -> (Server, koshi_core::ids::PaneId, ClientId, ViewerClient)
{
    let (mut runtime, _fake_pty_backend, client_id, viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    runtime.handle_pty_output(pane_id, &b"\n".repeat(40)); // push lines into history
    runtime.scroll_up(client_id, pane_id, 3);
    (runtime, pane_id, client_id, viewer)
}

/// Run `command` as if `client_id` had issued it from a keybinding, asserting it
/// was applied — a test that silently dispatched a rejected command would be
/// asserting against a state it never reached.
fn dispatch_test_command(runtime: &mut Server, client_id: ClientId, command: Command) {
    let command_envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::from_key_binding(client_id),
        SystemTime::now(),
        command,
    );
    let dispatch_result = runtime.dispatch(command_envelope);
    assert!(
        matches!(dispatch_result, CommandResult::Ok { .. }),
        "test setup: command was rejected: {dispatch_result:?}"
    );
}

#[test]
fn unbound_plain_key_passes_to_focused_pty() {
    let (mut runtime, fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'a'),
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
    let (mut runtime, fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let up_key_chord = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Up));

    // A shell leaves application-cursor-keys mode off, and reads `ESC [ A`.
    apply_key_press(&mut runtime, &mut viewer, up_key_chord, Instant::now());
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![b"\x1b[A".to_vec()]
    );

    // vim turns it on (DECCKM, `ESC [ ? 1 h`) and now reads `ESC O A` for the
    // same press. The pane's mode, not the press, picks the bytes.
    runtime.handle_pty_output(pane_id, b"\x1b[?1h");
    apply_key_press(&mut runtime, &mut viewer, up_key_chord, Instant::now());
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![b"\x1b[A".to_vec(), b"\x1bOA".to_vec()]
    );
}

#[test]
fn a_buffered_key_reaches_no_pane_at_all_even_after_focus_moves() {
    // An open sequence's chords belong to Koshi, not to any pane. Focus can move
    // while one waits — from something that is not a keypress at all, like a
    // `core:focus-pane` command over IPC — and the question "which pane gets the
    // buffered key" has one answer: none of them. Nothing typed into an open
    // sequence is ever written, so a stale recipient cannot be picked wrongly.
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let original_pane_id = get_only_pane_id(&runtime);
    let up_key_chord = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Up));
    let now = Instant::now();

    // A second pane, which takes focus. It runs vim: application-cursor-keys on.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    let focused_pane_id = get_focused_pane_id(&runtime, client_id);
    assert_ne!(focused_pane_id, original_pane_id);
    runtime.handle_pty_output(focused_pane_id, b"\x1b[?1h");

    // `<Up> x` makes a bare `<Up>` a prefix, so pressing it opens a sequence.
    configure_normal_keybinding(
        &mut viewer,
        KeySequence::from_first_and_rest(up_key_chord, vec![build_key_chord(ModFlags::NONE, 'x')]),
        ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    apply_key_press(&mut runtime, &mut viewer, up_key_chord, now);

    // Focus moves off that pane WITHOUT a keypress: a `core:focus-pane` command
    // from another source entirely. Only a keypress touches a pending sequence,
    // so the buffered `<Up>` is still open when the focused pane changes.
    let envelope = CommandEnvelope::from_parts(
        CommandId::new(),
        CommandSource::Mouse { client_id },
        SystemTime::now(),
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(original_pane_id),
            client_id: Some(client_id),
        }),
    );
    let dispatch_result = runtime.dispatch(envelope);
    assert_eq!(
        get_focused_pane_id(&runtime, client_id),
        original_pane_id,
        "{dispatch_result:?}"
    );

    // `z` continues nothing: it is discarded, and the sequence stands. Neither
    // pane sees a byte — not the buffered `<Up>`, not the `z`.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'z'),
        now,
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
        &mut runtime,
        &mut viewer,
        KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
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
    // a sequence waits — its bytes are applied on the same loop. It changes
    // nothing here: the buffered `<Up>` has no byte form to get wrong, because
    // it is never written in either mode.
    let (mut runtime, fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let up_key_chord = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Up));

    // `<Up> x` makes a bare `<Up>` a prefix, so pressing it opens a sequence
    // instead of passing straight through.
    configure_normal_keybinding(
        &mut viewer,
        KeySequence::from_first_and_rest(up_key_chord, vec![build_key_chord(ModFlags::NONE, 'x')]),
        ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );

    // Press `<Up>` while the pane is a plain shell: buffered, nothing written.
    let now = Instant::now();
    apply_key_press(&mut runtime, &mut viewer, up_key_chord, now);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );

    // The pane now turns application-cursor-keys mode ON, mid-sequence.
    runtime.handle_pty_output(pane_id, b"\x1b[?1h");

    // `z` continues nothing, so it is discarded and the sequence stands. The
    // pane sees neither the arrow nor the `z`, in either cursor mode.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'z'),
        now,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );

    // Completing the sequence fires the binding — still no bytes to the pane.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'x'),
        now,
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
    let (mut runtime, fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);

    // `<C-Right>` is a word-jump to a shell; dropping the Control would leave
    // it a plain Right and move one character instead.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        KeyChord::from_parts(ModFlags::CTRL, Key::Named(NamedKey::Right)),
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
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();
    // `<C-l>` locks in normal mode…
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'l'),
        now,
    );
    assert_eq!(
        runtime
            .get_session_for_client(client_id)
            .unwrap()
            .clients
            .get_client_by_id(client_id)
            .unwrap()
            .get_lock_mode(),
        LockMode::Locked
    );
    // …and the SAME chord is the reserved unlock in locked mode.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'l'),
        now,
    );
    assert_eq!(
        runtime
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
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'q'),
        Instant::now(),
    );
    // `auto-close-session` defaults off, so the client leaves and the session
    // keeps running.
    assert!(!is_client_attached(&runtime, client_id));
    assert!(!runtime.is_quit_requested());
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn quit_binding_detaches_the_client_in_locked_mode_too() {
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'l'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'q'),
        now,
    );
    assert!(!is_client_attached(&runtime, client_id));
    assert!(!runtime.is_quit_requested());
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn quit_binding_ends_the_session_when_auto_close_is_on() {
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    runtime.config.should_auto_close_session = true;
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'q'),
        Instant::now(),
    );
    // The sole client leaves, so the setting ends the session; teardown keeps
    // the graceful window.
    assert!(!is_client_attached(&runtime, client_id));
    assert!(runtime.is_quit_requested());
    assert!(!runtime.should_shutdown_immediately);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn continuous_resize_keeps_the_prefix_armed_for_repeat_presses() {
    let (mut runtime, _fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );

    // First resize: full `<C-s> <Left>` sequence.
    let pty_sizes_before = runtime.pty_size_by_pane_id.clone();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 's'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        now,
    );
    let pty_sizes_after_first_resize = runtime.pty_size_by_pane_id.clone();
    assert_ne!(pty_sizes_after_first_resize, pty_sizes_before);

    // The prefix stayed armed: `<Left>` alone fires the resize again…
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 's')))
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        now,
    );
    assert_ne!(runtime.pty_size_by_pane_id, pty_sizes_after_first_resize);

    // …and Escape puts the bar back to idle.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn one_shot_bindings_clear_the_whole_sequence_after_firing() {
    let (mut runtime, _fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    // `new-pane` is not continuous: after `<C-p> n` fires, nothing pends.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    assert_eq!(runtime.pty_handle_by_pane_id.len(), 2);
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn locked_mode_passes_non_unlock_keys_verbatim() {
    let (mut runtime, fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'l'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'x'),
        now,
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
    let (mut runtime, _fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 'p')))
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    assert_eq!(runtime.pty_handle_by_pane_id.len(), 2);
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn prefix_pending_never_expires() {
    let (mut runtime, fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    // A prefix-only sequence arms no deadline and outlives any wait: the
    // continuation hints stay up until the user presses another key.
    assert_eq!(viewer.next_key_wakeup(now), None);
    expire_pending_key_sequence(&mut runtime, &mut viewer, now + Duration::from_secs(3600));
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 'p')))
    );
}

#[test]
fn escape_cancels_a_pending_sequence_silently() {
    let (mut runtime, fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    // Neither the buffered prefix nor the Escape reaches the pane, and the
    // pending sequence is gone — the bar returns to its idle hints.
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
    let (mut runtime, fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();
    // `<C-p>` opens the pane prefix. `z` binds nothing under it: it goes
    // nowhere, and the prefix is still open — the shell must not see `Ctrl-P`
    // (history-back) or the `z`, because both were typed at Koshi.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'z'),
        now,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 'p')))
    );

    // The sequence is live, not merely remembered: `n` still completes it.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    assert_eq!(runtime.pty_handle_by_pane_id.len(), 2);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn directional_focus_binding_moves_focus_across_a_split() {
    let (mut runtime, _fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    // Split: the new right pane takes focus.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    let focused_pane_id_after_split = get_focused_pane_id(&runtime, client_id);

    // `<C-p> <Left>` focuses the left neighbor.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        now,
    );
    let focused_left_pane_id = get_focused_pane_id(&runtime, client_id);
    assert_ne!(focused_left_pane_id, focused_pane_id_after_split);

    // Focus is continuous, so the prefix stays armed: `<Right>` alone returns
    // to the right pane.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_named_key_chord(NamedKey::Right),
        now,
    );
    assert_eq!(
        get_focused_pane_id(&runtime, client_id),
        focused_pane_id_after_split
    );
}

#[test]
fn directional_new_pane_binding_splits_on_that_side() {
    let (mut runtime, _fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let original_pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();

    // `<C-p> h` opens a new pane on the LEFT of the focused one, and the new
    // pane takes focus.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'h'),
        now,
    );
    assert_eq!(runtime.pty_handle_by_pane_id.len(), 2);
    let new_pane_id = get_focused_pane_id(&runtime, client_id);
    assert_ne!(new_pane_id, original_pane_id);

    // The original pane is the new pane's RIGHT neighbor — exactly where a
    // left split puts it. A wrong split side leaves nothing to the right and
    // this focus move would stay put.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_named_key_chord(NamedKey::Right),
        now,
    );
    assert_eq!(get_focused_pane_id(&runtime, client_id), original_pane_id);
}

#[test]
fn the_viewers_configured_split_direction_reaches_the_new_pane() {
    let (mut runtime, _fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let original_pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();

    // The viewer folds `layout.new-pane-direction "down"` out of its own
    // `koshi.kdl`. Nothing else in the process holds a split direction, so if
    // this value does not travel with the fired binding the split comes out
    // rightward — the stock setting — and the assert below fails.
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

    let tab_id = get_active_tab_id(&runtime, client_id);
    let original_layout_tree = runtime
        .get_session_for_client(client_id)
        .expect("session")
        .tabs[&tab_id]
        .get_layout_tree()
        .clone();

    // `<C-p> n` is the direction-less new-pane binding.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );

    let new_pane_id = get_focused_pane_id(&runtime, client_id);
    assert_ne!(new_pane_id, original_pane_id);
    let expected_layout_tree = split_leaf(
        &original_layout_tree,
        original_pane_id,
        new_pane_id,
        Direction::Down,
    )
    .expect("split on the source leaf");
    assert_eq!(
        runtime
            .get_session_for_client(client_id)
            .expect("session")
            .tabs[&tab_id]
            .get_layout_tree(),
        &expected_layout_tree
    );
}

#[test]
fn a_user_bound_stacked_new_pane_key_builds_a_stack() {
    let (mut runtime, _fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let original_pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();

    // `new-pane-stacked` ships with no default key; a user binds their own.
    configure_normal_keybinding(
        &mut viewer,
        KeySequence::from(build_key_chord(ModFlags::ALT, 's')),
        ActionReference::from_core_action_name("new-pane-stacked").expect("valid name"),
        ActionArgs::None,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::ALT, 's'),
        now,
    );

    // The leaf becomes a two-member stack: the source collapses to a header
    // and the new pane is the expanded, focused member.
    assert_eq!(runtime.pty_handle_by_pane_id.len(), 2);
    let new_pane_id = get_focused_pane_id(&runtime, client_id);
    assert_ne!(new_pane_id, original_pane_id);
    let session = runtime.get_session_for_client(client_id).expect("session");
    let tab_id = session
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab();
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
    let (mut runtime, _fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::ALT, 'f'),
        now,
    );
    let snapshot = runtime.build_snapshot(client_id).expect("snapshot");
    assert_eq!(
        snapshot.session_snapshot.active_tab_snapshot.layout_mode,
        koshi_layout::mode::LayoutMode::Fullscreen {
            focused_pane_id: get_focused_pane_id(&runtime, client_id)
        }
    );

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::ALT, 'f'),
        now,
    );
    let snapshot = runtime.build_snapshot(client_id).expect("snapshot");
    assert_eq!(
        snapshot.session_snapshot.active_tab_snapshot.layout_mode,
        koshi_layout::mode::LayoutMode::Tiled
    );
}

fn get_focused_pane_id(runtime: &Server, client_id: ClientId) -> koshi_core::ids::PaneId {
    let session = runtime.get_session_for_client(client_id).expect("session");
    let client_state = session.clients.get_client_by_id(client_id).expect("client");
    client_state
        .get_focused_pane(client_state.get_active_tab())
        .expect("a focused pane")
}

/// The tab the client is looking at.
fn get_active_tab_id(runtime: &Server, client_id: ClientId) -> koshi_core::ids::TabId {
    runtime
        .get_session_for_client(client_id)
        .expect("session")
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab()
}

#[test]
fn resize_prefix_moves_a_live_split_border() {
    let (mut runtime, _fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    let pty_sizes_before = runtime.pty_size_by_pane_id.clone();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 's'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        now,
    );
    assert_ne!(runtime.pty_size_by_pane_id, pty_sizes_before);
}

#[test]
fn continuous_focus_rearm_walks_panes_with_repeated_arrows() {
    let (mut runtime, _fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    // Two splits: three panes across, focus on the right-most.
    for _ in 0..2 {
        apply_key_press(
            &mut runtime,
            &mut viewer,
            build_key_chord(ModFlags::CTRL, 'p'),
            now,
        );
        apply_key_press(
            &mut runtime,
            &mut viewer,
            build_key_chord(ModFlags::NONE, 'n'),
            now,
        );
    }
    let rightmost_pane_id = get_focused_pane_id(&runtime, client_id);

    // `<C-p> ←` moves one pane left and re-arms the prefix…
    let left_key_chord = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Left));
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(&mut runtime, &mut viewer, left_key_chord, now);
    let middle_pane_id = get_focused_pane_id(&runtime, client_id);
    assert_ne!(middle_pane_id, rightmost_pane_id);
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 'p')))
    );

    // …so a bare ← walks one further pane left.
    apply_key_press(&mut runtime, &mut viewer, left_key_chord, now);
    let leftmost_pane_id = get_focused_pane_id(&runtime, client_id);
    assert_ne!(leftmost_pane_id, middle_pane_id);
    assert_ne!(leftmost_pane_id, rightmost_pane_id);
}

#[test]
fn abandoned_rearmed_prefix_writes_nothing_to_the_pane() {
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    let focused_pane_id = get_focused_pane_id(&runtime, client_id);

    // Resize once, leave the re-armed prefix hanging, then cancel with Esc:
    // the re-armed prefix carries no fallback bytes, so the shell sees none.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 's'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
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
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    let focused_pane_id = get_focused_pane_id(&runtime, client_id);

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 's'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        now,
    );
    let sizes_after_one_resize = runtime.pty_size_by_pane_id.clone();

    // A re-armed prefix is an open sequence like any other, and captures like
    // one: `z` resizes nothing, so it is discarded — not passed to the shell —
    // and `<C-s>` stays armed.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'z'),
        now,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(focused_pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(runtime.pty_size_by_pane_id, sizes_after_one_resize);
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 's')))
    );

    // Still armed, so the next `<Left>` resizes again without re-pressing `<C-s>`.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_named_key_chord(NamedKey::Left),
        now,
    );
    assert_ne!(runtime.pty_size_by_pane_id, sizes_after_one_resize);

    // Escape is the way out, and it types nothing at the pane.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
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
    let (mut runtime, _fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    let focused_pane_id = get_focused_pane_id(&runtime, client_id);
    let original_pty_size = runtime.pty_size_by_pane_id[&focused_pane_id];

    // The focused pane touches the tab's right edge: `<C-s> l` has no right
    // border to grow through, so its left border moves right — it shrinks.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 's'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_named_key_chord(NamedKey::Right),
        now,
    );
    let resized_pty_size = runtime.pty_size_by_pane_id[&focused_pane_id];
    assert_eq!(
        resized_pty_size.column_count,
        original_pty_size.column_count - 1
    );
    assert_eq!(resized_pty_size.row_count, original_pty_size.row_count);
}

/// Bind one `normal`-mode sequence to `action` in the viewer's own keymap.
fn configure_normal_keybinding(
    viewer: &mut ViewerClient,
    sequence: KeySequence,
    action: ActionReference,
    action_arguments: ActionArgs,
) {
    configure_normal_keybindings(viewer, vec![(sequence, action, action_arguments)]);
}

/// Bind one `locked`-mode sequence to `action`, keeping the shipped locked
/// bindings (the unlock chord among them) beside it — a user layer that dropped
/// the unlock entry would be refused by conflict detection.
fn configure_locked_keybinding(
    viewer: &mut ViewerClient,
    sequence: KeySequence,
    action: ActionReference,
    action_arguments: ActionArgs,
) {
    let mut key_binding_by_sequence = KeybindingsConfig::default()
        .mode_bindings_by_name
        .remove(&ModeName::from_text("locked"))
        .expect("the shipped config binds locked mode")
        .bound_action_by_key_sequence;
    key_binding_by_sequence.insert(
        sequence,
        BoundAction {
            action_reference: action,
            action_arguments,
        },
    );
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("locked"),
        ModeBindings {
            bound_action_by_key_sequence: key_binding_by_sequence,
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

/// The client's current lock mode.
fn get_client_lock_mode(runtime: &Server, client_id: ClientId) -> LockMode {
    runtime
        .get_session_for_client(client_id)
        .expect("session")
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_lock_mode()
}

/// Bind every `(sequence, action, action_arguments)` triple in `bindings` under `normal`
/// mode in one `keybinding.kdl` the viewer reads. Reading the file replaces
/// the whole keybinding layer, so binding several sequences needs one call
/// with every entry, not several calls that would each overwrite the last.
fn configure_normal_keybindings(
    viewer: &mut ViewerClient,
    bindings: Vec<(KeySequence, ActionReference, ActionArgs)>,
) {
    let mut key_binding_by_sequence = BTreeMap::new();
    for (sequence, action, action_arguments) in bindings {
        key_binding_by_sequence.insert(
            sequence,
            BoundAction {
                action_reference: action,
                action_arguments,
            },
        );
    }
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("normal"),
        ModeBindings {
            bound_action_by_key_sequence: key_binding_by_sequence,
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
    let (mut runtime, fake_pty_backend, _client_id, _viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);

    // A press arriving for a client the session does not know resolves to no
    // pane, so nothing is written rather than landing on someone else's.
    runtime.handle_key_press(ClientId::new(), build_key_chord(ModFlags::NONE, 'x'));

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_key_writes_nothing_when_the_client_has_no_focused_pane() {
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let tab_id = runtime
        .get_session_for_client(client_id)
        .expect("session")
        .clients
        .get_client_by_id(client_id)
        .expect("client")
        .get_active_tab();
    runtime
        .get_session_for_client_mut(client_id)
        .expect("session")
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .remove_focused_pane(tab_id);

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'x'),
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
/// size, so it solves to no drawn pane and takes no keystroke.
#[test]
fn a_key_from_a_starving_sole_viewer_writes_nothing() {
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    runtime
        .get_session_for_client_mut(client_id)
        .expect("session")
        .clients
        .get_client_mut_by_id(client_id)
        .expect("client")
        .update_pane_area(Some(PaneArea::Starving));

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'x'),
        Instant::now(),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

/// A pane the tab has no room to draw takes no keystroke: the client cannot see
/// it, so a key aimed at the screen is not aimed at it. The terminal shrinks
/// below the pane's minimum, the pane is suppressed, and `l` reaches no shell.
#[test]
fn a_key_writes_nothing_when_the_focused_pane_is_suppressed() {
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);

    // Shrink the terminal until the sole pane no longer fits: a pane needs
    // MIN_PANE_SIZE plus its one-cell border, and 3x3 leaves less than that.
    runtime.handle_client_resize(
        client_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
        None,
    );
    assert!(
        runtime
            .build_snapshot(client_id)
            .expect("snapshot")
            .session_snapshot
            .active_tab_snapshot
            .are_all_panes_suppressed,
        "test setup: the sole pane must be suppressed at this size"
    );

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'l'),
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
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);

    runtime.handle_client_resize(
        client_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
        None,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'l'),
        Instant::now(),
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );

    runtime.handle_client_resize(
        client_id,
        Size {
            column_count: 80,
            row_count: 24,
        },
        None,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'l'),
        Instant::now(),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![b'l']]
    );
}

/// A plugin pane has no PTY behind it, so the bytes a chord encodes are not
/// its to read — even though it is focused, on screen, and its id has a live
/// PTY handle in the backend from when it was a terminal pane.
#[test]
fn a_key_writes_nothing_when_the_focused_pane_is_a_plugin_pane() {
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);

    // Re-file the focused pane's record under `Plugin`, keeping its id: the
    // layout leaf, the focus, and the PTY handle all stay exactly as they were,
    // so only the pane's KIND can explain a missing write.
    let session_id = runtime
        .get_session_for_client(client_id)
        .expect("session")
        .session_id;
    let session = runtime.session_by_id.get_mut(&session_id).expect("session");
    let created_at = session
        .panes
        .get_pane_record_by_id(pane_id)
        .expect("pane record")
        .get_created_at();
    session.panes.remove_pane_record(pane_id);
    session
        .panes
        .register_pane_record(PaneRecord::from_pane_kind(
            pane_id,
            PaneKind::Plugin {
                plugin_id: PluginId::new(),
            },
            created_at,
        ))
        .expect("re-inserting a removed pane id");

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'l'),
        Instant::now(),
    );

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

/// Zoom is per-client, so one client zooming a pane does not silence another
/// client's keys. A zooms its pane; B, tiled on the same tab, keeps typing into
/// the pane B can still see.
///
/// The guard asks "does the layout draw this pane FOR THIS CLIENT" — if it asked
/// the tab instead, B's pane would look hidden behind A's zoom and B's keystrokes
/// would vanish.
#[test]
fn one_client_zooming_does_not_stop_another_client_keys() {
    let (mut runtime, fake_pty_backend, first_client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    let original_pane_id = get_only_pane_id(&runtime);

    // Split so the tab has two panes; client A's focus lands on the new one.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    let focused_pane_id = get_focused_pane_id(&runtime, first_client_id);
    assert_ne!(focused_pane_id, original_pane_id);

    // Client B joins the tab, focused on the first pane.
    let (session_id, tab_id) = {
        let session = runtime
            .get_session_for_client(first_client_id)
            .expect("session");
        (
            session.session_id,
            session
                .clients
                .get_client_by_id(first_client_id)
                .expect("client")
                .get_active_tab(),
        )
    };
    let second_client_id = ClientId::new();
    let mut joining_client = Client::from_attachment(
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
    joining_client.update_focused_pane(tab_id, original_pane_id);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("session")
        .attach_client(joining_client);

    let mut second_viewer = build_viewer_client(&mut runtime, second_client_id);

    // Client A zooms its own pane, hiding `original_pane_id` — from A's view only.
    dispatch_test_command(&mut runtime, first_client_id, Command::TogglePaneFullscreen);

    // B types. B is tiled and can see `original_pane_id`, so its key lands there.
    apply_key_press(
        &mut runtime,
        &mut second_viewer,
        build_key_chord(ModFlags::NONE, 'y'),
        now,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(original_pane_id)
            .expect("writes"),
        vec![vec![b'y']]
    );

    // A types. A can see its zoomed pane, so its key lands there.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'z'),
        now,
    );
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(focused_pane_id)
            .expect("writes"),
        vec![vec![b'z']]
    );
}

/// Two clients view one tab holding a stack. Only the stack's active member is
/// drawn; the others collapse to a one-line header. Focus is per-client but the
/// active member is the tab's, so client B activating its member collapses the
/// pane client A still has focused — and client A's keys stop reaching it.
///
/// This is the case a suppression-only check misses: the collapsed pane is not
/// suppressed, it simply draws no content.
#[test]
fn a_key_writes_nothing_when_the_focused_pane_collapsed_to_a_stack_header() {
    let (mut runtime, fake_pty_backend, first_client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    let original_pane_id = get_only_pane_id(&runtime);

    // Stack a second pane onto the original pane. The new member becomes the
    // active one and takes client A's focus; the original pane collapses to a header.
    dispatch_test_command(
        &mut runtime,
        first_client_id,
        Command::NewPane(NewPaneArgs {
            source_pane_id: Some(original_pane_id),
            tab_id: None,
            direction: Direction::Right,
            should_stack: true,
            working_directory: None,
            spawn_spec: None,
            client_id: Some(first_client_id),
        }),
    );
    let stacked_pane_id = get_focused_pane_id(&runtime, first_client_id);
    assert_ne!(
        stacked_pane_id, original_pane_id,
        "test setup: the stacked pane took focus"
    );

    // Client B joins the same tab.
    let (session_id, tab_id) = {
        let session = runtime
            .get_session_for_client(first_client_id)
            .expect("session");
        (
            session.session_id,
            session
                .clients
                .get_client_by_id(first_client_id)
                .expect("client")
                .get_active_tab(),
        )
    };
    let second_client_id = ClientId::new();
    let mut joining_client = Client::from_attachment(
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
    joining_client.update_focused_pane(tab_id, stacked_pane_id);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("session")
        .attach_client(joining_client);

    // Client B focuses the other member, which activates it, so the stacked pane
    // client A still has focused collapses to a header.
    dispatch_test_command(
        &mut runtime,
        second_client_id,
        Command::FocusPane(FocusPaneArgs {
            focus_target: FocusTarget::Pane(original_pane_id),
            client_id: Some(second_client_id),
        }),
    );
    assert_eq!(
        get_focused_pane_id(&runtime, first_client_id),
        stacked_pane_id,
        "test setup: client_id A's focus did not move"
    );

    // Client A types at a pane that now draws nothing; client B types at the
    // member that is drawn.
    let mut second_viewer = build_viewer_client(&mut runtime, second_client_id);
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'z'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut second_viewer,
        build_key_chord(ModFlags::NONE, 'y'),
        now,
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
    let (mut runtime, fake_pty_backend, first_client_id, mut viewer) = build_test_runtime();
    let now = Instant::now();
    let original_pane_id = get_only_pane_id(&runtime);

    // Split: client A's focus moves to the new pane, leaving `original_pane`
    // unfocused by anyone yet.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'n'),
        now,
    );
    let first_client_pane_id = get_focused_pane_id(&runtime, first_client_id);
    assert_ne!(first_client_pane_id, original_pane_id);

    // Client B joins the same session, focused on the original pane — a
    // different pane than client A's.
    let (session_id, tab_id) = {
        let session = runtime
            .get_session_for_client(first_client_id)
            .expect("session");
        (
            session.session_id,
            session
                .clients
                .get_client_by_id(first_client_id)
                .expect("client")
                .get_active_tab(),
        )
    };
    let second_client_id = ClientId::new();
    let mut joining_client = Client::from_attachment(
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
    joining_client.update_focused_pane(tab_id, original_pane_id);
    runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("session")
        .attach_client(joining_client);

    // Each viewer holds its own keymap and its own open sequence, so client B
    // gets one of its own.
    let mut second_viewer = build_viewer_client(&mut runtime, second_client_id);

    // Client A opens the pane prefix and leaves it hanging...
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        now,
    );
    // ...client B, meanwhile, sends an unrelated unbound key straight through
    // on its own (different) pane.
    apply_key_press(
        &mut runtime,
        &mut second_viewer,
        build_key_chord(ModFlags::NONE, 'z'),
        now,
    );

    // Only `z` reaches client B's own pane — never client A's buffered
    // `<C-p>` byte, and client A's held pane sees nothing at all.
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
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 'p'))),
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
    // Each viewer owns the sequence it is typing, so one holding a prefix open
    // cannot make another viewer's next key continue it.
    let (mut runtime, _fake_pty_backend, first_client_id, mut viewer) = build_test_runtime();
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'p'),
        Instant::now(),
    );

    // Client B joins the same session with no sequence of its own.
    let (session_id, tab_id) = {
        let session = runtime
            .get_session_for_client(first_client_id)
            .expect("session");
        (
            session.session_id,
            session
                .clients
                .get_client_by_id(first_client_id)
                .expect("client")
                .get_active_tab(),
        )
    };
    let second_client_id = ClientId::new();
    let mut joining_client = Client::from_attachment(
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
    joining_client.update_focused_pane(tab_id, get_only_pane_id(&runtime));
    runtime
        .session_by_id
        .get_mut(&session_id)
        .expect("session")
        .attach_client(joining_client);
    let mut second_viewer = build_viewer_client(&mut runtime, second_client_id);

    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 'p'))),
        "client A is mid-sequence"
    );
    assert_eq!(
        second_viewer.get_pending_key_sequence().cloned(),
        None,
        "client B has nothing open"
    );

    // So B's `n` is its own key, not the continuation that would fire
    // `<C-p> n` — it types, and A's sequence is still waiting.
    assert_eq!(
        second_viewer.resolve_key(build_key_chord(ModFlags::NONE, 'n'), Instant::now()),
        KeyOutcome::PassThrough(build_key_chord(ModFlags::NONE, 'n'))
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 'p'))),
        "A's sequence outlives B's keypress"
    );
}

#[test]
fn a_sequence_grows_to_the_chord_depth_cap_and_no_further() {
    let (mut runtime, fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    // A 4-chord binding, exactly the default `max_chord_depth`. The cap bounds
    // pending state without a check on the input path: a sequence only grows
    // while a longer live binding still starts with it, and the merge drops any
    // binding past the cap, so no pending sequence can outgrow it.
    let long_key_sequence = KeySequence::from_first_and_rest(
        build_key_chord(ModFlags::CTRL, 'y'),
        vec![
            build_key_chord(ModFlags::NONE, 'a'),
            build_key_chord(ModFlags::NONE, 'b'),
            build_key_chord(ModFlags::NONE, 'c'),
        ],
    );
    configure_normal_keybinding(
        &mut viewer,
        long_key_sequence.clone(),
        ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    let tab_count_before = runtime
        .list_sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    let now = Instant::now();
    for chord in long_key_sequence.list_chords() {
        apply_key_press(&mut runtime, &mut viewer, *chord, now);
    }

    // The full-depth binding fires, the sequence closes, and nothing along the
    // way was typed at the pane.
    assert_eq!(
        runtime
            .list_sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tab_count_before + 1
    );
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
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();
    // A locked-mode sequence of the user's own: `<C-x> a`. Pressing `<C-x>`
    // opens it, so the client is locked AND mid-sequence — the state the unlock
    // guarantee has to survive.
    configure_locked_keybinding(
        &mut viewer,
        KeySequence::from_first_and_rest(
            build_key_chord(ModFlags::CTRL, 'x'),
            vec![build_key_chord(ModFlags::NONE, 'a')],
        ),
        ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'l'),
        now,
    );
    assert_eq!(get_client_lock_mode(&runtime, client_id), LockMode::Locked);
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'x'),
        now,
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 'x')))
    );

    // The unlock chord resolves ahead of the keymap and ahead of the open
    // sequence: the client unlocks, the held `<C-x>` is dropped rather than
    // typed at the pane, and no pending sequence survives into normal mode.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'l'),
        now,
    );
    assert_eq!(get_client_lock_mode(&runtime, client_id), LockMode::Normal);
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
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let now = Instant::now();
    // `<C-x> <C-l>` in locked mode: the unlock resolves at the `<C-l>` wherever
    // it is pressed, so this binding can never fire. The config layer knows it
    // is dead and drops it, which is what keeps the two halves honest — if the
    // merge admitted it, `<C-x>` would become a live prefix that captures the
    // keyboard and offers a hint-bar continuation that silently unlocks.
    configure_locked_keybinding(
        &mut viewer,
        KeySequence::from_first_and_rest(
            build_key_chord(ModFlags::CTRL, 'x'),
            vec![KeybindingsConfig::RESERVED_UNLOCK],
        ),
        ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'l'),
        now,
    );
    assert_eq!(get_client_lock_mode(&runtime, client_id), LockMode::Locked);
    let tab_count_before = runtime
        .list_sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    // The dead binding wins no key: `<C-x>` opens no sequence and passes to the
    // pane verbatim, exactly as locked mode passes every unbound key.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'x'),
        now,
    );
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        vec![vec![0x18]]
    );

    // And the unlock still unlocks — it never became a continuation of anything.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'l'),
        now,
    );
    assert_eq!(get_client_lock_mode(&runtime, client_id), LockMode::Normal);
    assert_eq!(
        runtime
            .list_sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tab_count_before,
        "the dead binding's action must never run"
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
    let (mut runtime, _fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    // `<C-y>` alone is both a complete binding and a prefix of `<C-y> x`, so
    // pressing it arms an ambiguity deadline.
    configure_normal_keybindings(
        &mut viewer,
        vec![
            (
                KeySequence::from_first_and_rest(build_key_chord(ModFlags::CTRL, 'y'), Vec::new()),
                ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
                ActionArgs::None,
            ),
            (
                KeySequence::from_first_and_rest(
                    build_key_chord(ModFlags::CTRL, 'y'),
                    vec![build_key_chord(ModFlags::NONE, 'x')],
                ),
                ActionReference::from_core_action_name("unlock").expect("valid core action name"),
                ActionArgs::None,
            ),
        ],
    );
    let now = Instant::now();
    let tab_count_before = runtime
        .list_sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'y'),
        now,
    );
    let ambiguity_deadline = now + get_chord_timeout(&viewer);
    expire_pending_key_sequence(
        &mut runtime,
        &mut viewer,
        ambiguity_deadline - Duration::from_millis(1),
    );

    assert_eq!(
        runtime
            .list_sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tab_count_before
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 'y')))
    );
}

#[test]
fn expire_key_sequences_at_the_deadline_fires_the_ambiguous_bindings_exact_match() {
    let (mut runtime, _fake_pty_backend, _client_id, mut viewer) = build_test_runtime();
    configure_normal_keybindings(
        &mut viewer,
        vec![
            (
                KeySequence::from_first_and_rest(build_key_chord(ModFlags::CTRL, 'y'), Vec::new()),
                ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
                ActionArgs::None,
            ),
            (
                KeySequence::from_first_and_rest(
                    build_key_chord(ModFlags::CTRL, 'y'),
                    vec![build_key_chord(ModFlags::NONE, 'x')],
                ),
                ActionReference::from_core_action_name("unlock").expect("valid core action name"),
                ActionArgs::None,
            ),
        ],
    );
    let now = Instant::now();
    let tab_count_before = runtime
        .list_sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'y'),
        now,
    );
    let ambiguity_deadline = now + get_chord_timeout(&viewer);
    expire_pending_key_sequence(&mut runtime, &mut viewer, ambiguity_deadline);

    assert_eq!(
        runtime
            .list_sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tab_count_before + 1
    );
    assert_eq!(viewer.get_pending_key_sequence().cloned(), None);
}

#[test]
fn a_held_exact_binding_survives_a_key_it_cannot_use_and_fires_at_its_deadline() {
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    // `<C-y>` alone is both a complete binding and a prefix of `<C-y> x`, so
    // pressing it opens a sequence that carries an ambiguity deadline.
    configure_normal_keybindings(
        &mut viewer,
        vec![
            (
                KeySequence::from_first_and_rest(build_key_chord(ModFlags::CTRL, 'y'), Vec::new()),
                ActionReference::from_core_action_name("new-tab").expect("valid core action name"),
                ActionArgs::None,
            ),
            (
                KeySequence::from_first_and_rest(
                    build_key_chord(ModFlags::CTRL, 'y'),
                    vec![build_key_chord(ModFlags::NONE, 'x')],
                ),
                ActionReference::from_core_action_name("unlock").expect("valid core action name"),
                ActionArgs::None,
            ),
        ],
    );
    let now = Instant::now();
    let tab_count_before = runtime
        .list_sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'y'),
        now,
    );
    // `z` extends `<C-y>` into nothing, so it is discarded — the sequence is not
    // abandoned by a key it cannot use, and its deadline still stands.
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'z'),
        now,
    );
    assert_eq!(
        runtime
            .list_sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tab_count_before,
        "the held binding waits for its ambiguity_deadline, not for a mismatch"
    );
    assert_eq!(
        viewer.get_pending_key_sequence().cloned(),
        Some(KeySequence::from(build_key_chord(ModFlags::CTRL, 'y')))
    );

    // The deadline decides: `<C-y>`'s own binding fires, and the client lands on
    // the new tab. Neither the held chord nor the discarded `z` was ever typed.
    let ambiguity_deadline = now + get_chord_timeout(&viewer);
    expire_pending_key_sequence(&mut runtime, &mut viewer, ambiguity_deadline);
    assert_eq!(
        runtime
            .list_sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tab_count_before + 1
    );
    let new_pane_id = get_focused_pane_id(&runtime, client_id);
    assert_ne!(
        new_pane_id, pane_id,
        "new-tab must have switched focus to a new pane"
    );
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
    let (mut runtime, pane_id, client_id, mut viewer) = build_runtime_with_scrolled_client();
    assert_eq!(get_client_scroll_offset(&runtime, client_id, pane_id), 3);

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(get_client_scroll_offset(&runtime, client_id, pane_id), 0);
}

#[test]
fn typing_leaves_the_view_parked_when_scroll_on_input_is_off() {
    let (mut runtime, pane_id, client_id, mut viewer) = build_runtime_with_scrolled_client();
    runtime.client_config.scrollback.should_scroll_to_input = false;

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(get_client_scroll_offset(&runtime, client_id, pane_id), 3);
}

#[test]
fn typing_on_the_alternate_screen_leaves_the_view_to_the_program() {
    let (mut runtime, pane_id, client_id, mut viewer) = build_runtime_with_scrolled_client();
    runtime.handle_pty_output(pane_id, b"\x1b[?1049h"); // enter the alternate screen

    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(get_client_scroll_offset(&runtime, client_id, pane_id), 3);
}

#[test]
fn pasting_snaps_a_scrolled_up_view_back_to_live_output() {
    let (mut runtime, pane_id, client_id, _viewer) = build_runtime_with_scrolled_client();

    runtime.handle_host_paste(client_id, "ls\n");
    assert_eq!(get_client_scroll_offset(&runtime, client_id, pane_id), 0);
}

#[test]
fn an_empty_host_paste_writes_nothing() {
    let (mut runtime, fake_pty_backend, client_id, _viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);

    // Selecting nothing and hitting the OS paste key hands the session an
    // empty string; no empty write reaches the shell.
    runtime.handle_host_paste(client_id, "");

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_host_paste_from_an_unknown_client_writes_nothing() {
    let (mut runtime, fake_pty_backend, _client_id, _viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);

    // A paste arriving for a client the session does not know has no lock mode
    // to read, so nothing is written rather than landing on someone else's pane.
    runtime.handle_host_paste(ClientId::new(), "ls");

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_host_paste_still_reaches_the_pane_while_the_client_is_locked() {
    let (mut runtime, fake_pty_backend, client_id, mut viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    apply_key_press(
        &mut runtime,
        &mut viewer,
        build_key_chord(ModFlags::CTRL, 'l'),
        Instant::now(),
    );
    assert_eq!(get_client_lock_mode(&runtime, client_id), LockMode::Locked);

    runtime.handle_host_paste(client_id, "ls");

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
    let (mut runtime, fake_pty_backend, client_id, _viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);

    // 3x3 leaves less than MIN_PANE_SIZE plus the pane's one-cell border, so
    // the sole pane draws no content and a paste is aimed at nothing.
    runtime.handle_client_resize(
        client_id,
        Size {
            column_count: 3,
            row_count: 3,
        },
        None,
    );
    assert!(
        runtime
            .build_snapshot(client_id)
            .expect("snapshot")
            .session_snapshot
            .active_tab_snapshot
            .are_all_panes_suppressed,
        "test setup: the sole pane must be suppressed at this size"
    );

    runtime.handle_host_paste(client_id, "ls");

    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn a_bound_action_the_session_does_not_know_dispatches_nothing() {
    let (mut runtime, fake_pty_backend, client_id, _viewer) = build_test_runtime();
    let pane_id = get_only_pane_id(&runtime);
    let tab_count_before = runtime
        .list_sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    // A viewer keymap naming an action this session's registry has no entry
    // for: the action resolves to no plan, so no command is dispatched and the
    // chord is not written to the pane either.
    runtime.handle_bound_action(
        client_id,
        BoundAction {
            action_reference: ActionReference::from_core_action_name("no-such-action")
                .expect("valid core action name"),
            action_arguments: ActionArgs::None,
        },
        Direction::Right,
    );

    assert_eq!(
        runtime
            .list_sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tab_count_before
    );
    assert_eq!(runtime.pty_handle_by_pane_id.len(), 1);
    assert_eq!(
        fake_pty_backend
            .list_pane_write_bytes(pane_id)
            .expect("writes"),
        Vec::<Vec<u8>>::new()
    );
}
