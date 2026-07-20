//! End-to-end default-keymap tests: passthrough, lock escape, prefix display,
//! multi-chord dispatch, timeout fallback, open-sequence capture, and pane
//! resize.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc};

use koshi_config::conflict::{KeyMapLayer, KeymapVerdict, LayerOrigin};
use koshi_config::layer::PartialKeybindingsConfig;
use koshi_config::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::action::ActionRef;
use koshi_core::command::{Command, CommandResult, FocusPaneArgs, FocusTarget, NewPaneArgs};
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{PluginId, SessionId};
use koshi_core::key::{Key, ModFlags};
use koshi_core::resolve::ActionArgs;
use koshi_layout::tree::{LayoutNode, SplitNode};
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_pane::pane::state::PaneRecord;
use koshi_session::client::Client;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage};
use crate::runtime::hints::KeymapHintCatalog;
use crate::runtime::state::Runtime;

fn runtime() -> (Runtime, Arc<FakePtyBackend>, ClientId) {
    let fake = Arc::new(FakePtyBackend::new());
    let (tx, rx) = mpsc::channel();
    let mut runtime = Runtime::new(
        fake.clone(),
        Arc::new(NullSnapshotProvider),
        Arc::new(NullStorage),
        rx,
        tx,
        TerminalCleanupGuard::new(),
        Direction::Right,
    );
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    (runtime, fake, client)
}

fn chord(mods: ModFlags, key: char) -> KeyChord {
    KeyChord::new(mods, Key::Char(key))
}

/// An unmodified named key, for the arrows the default focus and resize
/// sequences continue with.
fn named(key: NamedKey) -> KeyChord {
    KeyChord::new(ModFlags::NONE, Key::Named(key))
}

fn only_pane(runtime: &Runtime) -> koshi_core::ids::PaneId {
    *runtime.pty_handles.keys().next().expect("one pane")
}

/// The client's scroll offset for the pane — `0` means the view follows live output.
fn scroll_offset(runtime: &Runtime, client: ClientId, pane: koshi_core::ids::PaneId) -> usize {
    runtime
        .sessions()
        .values()
        .next()
        .expect("one session")
        .clients
        .get(client)
        .expect("client present")
        .scroll_offset(pane)
}

/// A bootstrapped runtime whose one client has scrolled its view 3 lines up into a
/// pane's history — the parked-view starting point the `scroll-on-input` tests share.
fn runtime_scrolled_up() -> (Runtime, koshi_core::ids::PaneId, ClientId) {
    let (mut runtime, _fake, client) = runtime();
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, &b"\n".repeat(40)); // push lines into history
    runtime.scroll_up(client, pane, 3);
    (runtime, pane, client)
}

/// Run `command` as if `client` had issued it from a keybinding, asserting it
/// was applied — a test that silently dispatched a rejected command would be
/// asserting against a state it never reached.
fn dispatch(runtime: &mut Runtime, client: ClientId, command: Command) {
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::key_binding(client),
        SystemTime::now(),
        command,
    );
    let result = runtime.dispatch(envelope);
    assert!(
        matches!(result, CommandResult::Ok { .. }),
        "test setup: command was rejected: {result:?}"
    );
}

#[test]
fn unbound_plain_key_passes_to_focused_pty() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'a'), Instant::now());
    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![b'a']]);
}

#[test]
fn an_unbound_arrow_follows_the_focused_panes_application_cursor_mode() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let up = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Up));

    // A shell leaves application-cursor-keys mode off, and reads `ESC [ A`.
    runtime.handle_key_input(client, up, Instant::now());
    assert_eq!(fake.writes(pane).expect("writes"), vec![b"\x1b[A".to_vec()]);

    // vim turns it on (DECCKM, `ESC [ ? 1 h`) and now reads `ESC O A` for the
    // same press. The pane's mode, not the press, picks the bytes.
    runtime.handle_pty_output(pane, b"\x1b[?1h");
    runtime.handle_key_input(client, up, Instant::now());
    assert_eq!(
        fake.writes(pane).expect("writes"),
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
    let (mut runtime, fake, client) = runtime();
    let first = only_pane(&runtime);
    let up = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Up));
    let now = Instant::now();

    // A second pane, which takes focus. It runs vim: application-cursor-keys on.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);
    let second = focused_pane(&runtime, client);
    assert_ne!(second, first);
    runtime.handle_pty_output(second, b"\x1b[?1h");

    // `<Up> x` makes a bare `<Up>` a prefix, so pressing it opens a sequence.
    bind_normal(
        &mut runtime,
        KeySequence::new(up, vec![chord(ModFlags::NONE, 'x')]),
        ActionRef::core("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    runtime.handle_key_input(client, up, now);

    // Focus moves off that pane WITHOUT a keypress: a `core:focus-pane` command
    // from another source entirely. Only a keypress touches a pending sequence,
    // so the buffered `<Up>` is still open when the focused pane changes.
    let envelope = CommandEnvelope::new(
        CommandId::new(),
        CommandSource::Mouse { client_id: client },
        SystemTime::now(),
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(first),
            client: Some(client),
        }),
    );
    let result = runtime.dispatch(envelope);
    assert_eq!(focused_pane(&runtime, client), first, "{result:?}");

    // `z` continues nothing: it is discarded, and the sequence stands. Neither
    // pane sees a byte — not the buffered `<Up>`, not the `z`.
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'z'), now);
    assert_eq!(fake.writes(second).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(fake.writes(first).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(up)),
        "the open sequence outlives a key it cannot use"
    );

    // Escape leaves the sequence, and still nothing is typed at either pane.
    runtime.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    assert_eq!(fake.writes(second).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(fake.writes(first).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn a_buffered_arrow_is_never_written_even_when_its_pane_flips_cursor_mode() {
    // A pane can turn application-cursor-keys mode on from its own output while
    // a sequence waits — its bytes are applied on the same loop. It changes
    // nothing here: the buffered `<Up>` has no byte form to get wrong, because
    // it is never written in either mode.
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let up = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Up));

    // `<Up> x` makes a bare `<Up>` a prefix, so pressing it opens a sequence
    // instead of passing straight through.
    bind_normal(
        &mut runtime,
        KeySequence::new(up, vec![chord(ModFlags::NONE, 'x')]),
        ActionRef::core("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );

    // Press `<Up>` while the pane is a plain shell: buffered, nothing written.
    let now = Instant::now();
    runtime.handle_key_input(client, up, now);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());

    // The pane now turns application-cursor-keys mode ON, mid-sequence.
    runtime.handle_pty_output(pane, b"\x1b[?1h");

    // `z` continues nothing, so it is discarded and the sequence stands. The
    // pane sees neither the arrow nor the `z`, in either cursor mode.
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'z'), now);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());

    // Completing the sequence fires the binding — still no bytes to the pane.
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'x'), now);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn a_modified_arrow_keeps_its_modifier_on_the_way_to_the_pane() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);

    // `<C-Right>` is a word-jump to a shell; dropping the Control would leave
    // it a plain Right and move one character instead.
    runtime.handle_key_input(
        client,
        KeyChord::new(ModFlags::CTRL, Key::Named(NamedKey::Right)),
        Instant::now(),
    );
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![b"\x1b[1;5C".to_vec()]
    );
}

#[test]
fn the_lock_chord_flips_the_client_both_ways_without_pty_bytes() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    // `<C-l>` locks in normal mode…
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), now);
    assert_eq!(
        runtime
            .session_for_client(client)
            .unwrap()
            .clients
            .get(client)
            .unwrap()
            .lock_mode(),
        LockMode::Locked
    );
    // …and the SAME chord is the reserved unlock in locked mode.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), now);
    assert_eq!(
        runtime
            .session_for_client(client)
            .unwrap()
            .clients
            .get(client)
            .unwrap()
            .lock_mode(),
        LockMode::Normal
    );
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn quit_binding_fires_in_normal_mode() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'q'), Instant::now());
    assert!(runtime.quit_requested());
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn quit_binding_fires_in_locked_mode_too() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), now);
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'q'), now);
    assert!(runtime.quit_requested());
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn continuous_resize_keeps_the_prefix_armed_for_repeat_presses() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);

    // First resize: full `<C-s> <Left>` sequence.
    let sizes_start = runtime.pty_sizes.clone();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 's'), now);
    runtime.handle_key_input(client, named(NamedKey::Left), now);
    let sizes_once = runtime.pty_sizes.clone();
    assert_ne!(sizes_once, sizes_start);

    // The prefix stayed armed: `<Left>` alone fires the resize again…
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(chord(ModFlags::CTRL, 's')))
    );
    runtime.handle_key_input(client, named(NamedKey::Left), now);
    assert_ne!(runtime.pty_sizes, sizes_once);

    // …and Escape puts the bar back to idle.
    runtime.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
}

#[test]
fn one_shot_bindings_clear_the_whole_sequence_after_firing() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    // `new-pane` is not continuous: after `<C-p> n` fires, nothing pends.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);
    assert_eq!(runtime.pty_handles.len(), 2);
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
}

#[test]
fn locked_mode_passes_non_unlock_keys_verbatim() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'x'), now);
    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![b'x']]);
}

#[test]
fn pane_prefix_updates_snapshot_then_new_pane_fires() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p')))
    );
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);
    assert_eq!(runtime.pty_handles.len(), 2);
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
}

#[test]
fn prefix_pending_never_expires() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    // A prefix-only sequence arms no deadline and outlives any wait: the
    // continuation hints stay up until the user presses another key.
    assert_eq!(runtime.next_key_wakeup(now), None);
    runtime.expire_key_sequences(now + Duration::from_secs(3600));
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
    assert!(runtime
        .build_snapshot(client)
        .unwrap()
        .client
        .pending_sequence
        .is_some());
}

#[test]
fn escape_cancels_a_pending_sequence_silently() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    // Neither the buffered prefix nor the Escape reaches the pane, and the
    // pending sequence is gone — the bar returns to its idle hints.
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
}

#[test]
fn an_unmatched_continuation_is_discarded_and_the_sequence_stands() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    // `<C-p>` opens the pane prefix. `z` binds nothing under it: it goes
    // nowhere, and the prefix is still open — the shell must not see `Ctrl-P`
    // (history-back) or the `z`, because both were typed at Koshi.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'z'), now);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p')))
    );

    // The sequence is live, not merely remembered: `n` still completes it.
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);
    assert_eq!(runtime.pty_handles.len(), 2);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn directional_focus_binding_moves_focus_across_a_split() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    // Split: the new right pane takes focus.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);
    let focused_after_split = focused_pane(&runtime, client);

    // `<C-p> <Left>` focuses the left neighbor.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, named(NamedKey::Left), now);
    let focused_left = focused_pane(&runtime, client);
    assert_ne!(focused_left, focused_after_split);

    // Focus is continuous, so the prefix stays armed: `<Right>` alone returns
    // to the right pane.
    runtime.handle_key_input(client, named(NamedKey::Right), now);
    assert_eq!(focused_pane(&runtime, client), focused_after_split);
}

#[test]
fn directional_new_pane_binding_splits_on_that_side() {
    let (mut runtime, _fake, client) = runtime();
    let original = only_pane(&runtime);
    let now = Instant::now();

    // `<C-p> h` opens a new pane on the LEFT of the focused one, and the new
    // pane takes focus.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'h'), now);
    assert_eq!(runtime.pty_handles.len(), 2);
    let new_pane = focused_pane(&runtime, client);
    assert_ne!(new_pane, original);

    // The original pane is the new pane's RIGHT neighbor — exactly where a
    // left split puts it. A wrong split side leaves nothing to the right and
    // this focus move would stay put.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, named(NamedKey::Right), now);
    assert_eq!(focused_pane(&runtime, client), original);
}

#[test]
fn a_user_bound_stacked_new_pane_key_builds_a_stack() {
    let (mut runtime, _fake, client) = runtime();
    let original = only_pane(&runtime);
    let now = Instant::now();

    // `new-pane-stacked` ships with no default key; a user binds their own.
    bind_normal(
        &mut runtime,
        KeySequence::from(chord(ModFlags::ALT, 's')),
        ActionRef::core("new-pane-stacked").expect("valid name"),
        ActionArgs::None,
    );
    runtime.handle_key_input(client, chord(ModFlags::ALT, 's'), now);

    // The leaf becomes a two-member stack: the source collapses to a header
    // and the new pane is the expanded, focused member.
    assert_eq!(runtime.pty_handles.len(), 2);
    let new_pane = focused_pane(&runtime, client);
    assert_ne!(new_pane, original);
    let session = runtime.session_for_client(client).expect("session");
    let tab = session.clients.get(client).expect("client").active_tab();
    assert_eq!(
        session.tabs[&tab].layout(),
        &LayoutNode::Split(SplitNode::stack(vec![original, new_pane], 1))
    );
}

#[test]
fn fullscreen_binding_toggles_the_layout_mode() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);

    runtime.handle_key_input(client, chord(ModFlags::ALT, 'f'), now);
    let snap = runtime.build_snapshot(client).expect("snapshot");
    assert_eq!(
        snap.session.active_tab.layout_mode,
        koshi_layout::mode::LayoutMode::Fullscreen {
            focused: focused_pane(&runtime, client)
        }
    );

    runtime.handle_key_input(client, chord(ModFlags::ALT, 'f'), now);
    let snap = runtime.build_snapshot(client).expect("snapshot");
    assert_eq!(
        snap.session.active_tab.layout_mode,
        koshi_layout::mode::LayoutMode::Tiled
    );
}

fn focused_pane(runtime: &Runtime, client: ClientId) -> koshi_core::ids::PaneId {
    let session = runtime.session_for_client(client).expect("session");
    let state = session.clients.get(client).expect("client");
    state
        .focused_pane(state.active_tab())
        .expect("a focused pane")
}

#[test]
fn resize_prefix_moves_a_live_split_border() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);
    let sizes_before = runtime.pty_sizes.clone();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 's'), now);
    runtime.handle_key_input(client, named(NamedKey::Left), now);
    assert_ne!(runtime.pty_sizes, sizes_before);
}

#[test]
fn continuous_focus_rearm_walks_panes_with_repeated_arrows() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    // Two splits: three panes across, focus on the right-most.
    for _ in 0..2 {
        runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
        runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);
    }
    let rightmost = focused_pane(&runtime, client);

    // `<C-p> ←` moves one pane left and re-arms the prefix…
    let left = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Left));
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, left, now);
    let middle = focused_pane(&runtime, client);
    assert_ne!(middle, rightmost);
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p')))
    );

    // …so a bare ← walks one further pane left.
    runtime.handle_key_input(client, left, now);
    let leftmost = focused_pane(&runtime, client);
    assert_ne!(leftmost, middle);
    assert_ne!(leftmost, rightmost);
}

#[test]
fn abandoned_rearmed_prefix_writes_nothing_to_the_pane() {
    let (mut runtime, fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);
    let focused = focused_pane(&runtime, client);

    // Resize once, leave the re-armed prefix hanging, then cancel with Esc:
    // the re-armed prefix carries no fallback bytes, so the shell sees none.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 's'), now);
    runtime.handle_key_input(client, named(NamedKey::Left), now);
    runtime.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    assert_eq!(fake.writes(focused).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
}

#[test]
fn an_unmatched_key_under_a_rearmed_prefix_is_discarded_and_it_stays_armed() {
    let (mut runtime, fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);
    let focused = focused_pane(&runtime, client);

    runtime.handle_key_input(client, chord(ModFlags::CTRL, 's'), now);
    runtime.handle_key_input(client, named(NamedKey::Left), now);
    let sizes_after_one_resize = runtime.pty_sizes.clone();

    // A re-armed prefix is an open sequence like any other, and captures like
    // one: `z` resizes nothing, so it is discarded — not passed to the shell —
    // and `<C-s>` stays armed.
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'z'), now);
    assert_eq!(fake.writes(focused).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(runtime.pty_sizes, sizes_after_one_resize);
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(chord(ModFlags::CTRL, 's')))
    );

    // Still armed, so the next `<Left>` resizes again without re-pressing `<C-s>`.
    runtime.handle_key_input(client, named(NamedKey::Left), now);
    assert_ne!(runtime.pty_sizes, sizes_after_one_resize);

    // Escape is the way out, and it types nothing at the pane.
    runtime.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    assert_eq!(fake.writes(focused).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
}

#[test]
fn resize_binding_at_the_tab_edge_moves_the_opposite_border() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), now);
    let focused = focused_pane(&runtime, client);
    let before = runtime.pty_sizes[&focused];

    // The focused pane touches the tab's right edge: `<C-s> l` has no right
    // border to grow through, so its left border moves right — it shrinks.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 's'), now);
    runtime.handle_key_input(client, named(NamedKey::Right), now);
    let after = runtime.pty_sizes[&focused];
    assert_eq!(after.cols, before.cols - 1);
    assert_eq!(after.rows, before.rows);
}

/// Bind one `normal`-mode sequence to `action` via [`Runtime::reload_keybindings`].
fn bind_normal(runtime: &mut Runtime, sequence: KeySequence, action: ActionRef, args: ActionArgs) {
    bind_normal_all(runtime, vec![(sequence, action, args)]);
}

/// Bind one `locked`-mode sequence to `action`, keeping the shipped locked
/// bindings (the unlock chord among them) beside it — a user layer that dropped
/// the unlock entry would be refused by conflict detection.
fn bind_locked(runtime: &mut Runtime, sequence: KeySequence, action: ActionRef, args: ActionArgs) {
    let mut keys = KeybindingsConfig::default()
        .modes
        .remove(&ModeName::new("locked"))
        .expect("the shipped config binds locked mode")
        .keys;
    keys.insert(sequence, BoundAction { action, args });
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new("locked"),
        ModeBindings {
            keys,
            removed: BTreeSet::new(),
        },
    );
    let outcome = runtime.reload_keybindings(PartialKeybindingsConfig {
        modes: Some(modes),
        ..PartialKeybindingsConfig::default()
    });
    assert_eq!(
        outcome.report.verdict(),
        KeymapVerdict::Apply,
        "test setup: the candidate binding must apply cleanly"
    );
}

/// The client's current lock mode.
fn lock_mode(runtime: &Runtime, client: ClientId) -> LockMode {
    runtime
        .session_for_client(client)
        .expect("session")
        .clients
        .get(client)
        .expect("client")
        .lock_mode()
}

/// Bind every `(sequence, action, args)` triple in `bindings` under `normal`
/// mode in a single [`Runtime::reload_keybindings`] call. A keybinding reload
/// replaces the whole keybinding layer, so binding several sequences needs
/// one call with every entry, not several calls that would each overwrite
/// the last.
fn bind_normal_all(runtime: &mut Runtime, bindings: Vec<(KeySequence, ActionRef, ActionArgs)>) {
    let mut keys = BTreeMap::new();
    for (sequence, action, args) in bindings {
        keys.insert(sequence, BoundAction { action, args });
    }
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new("normal"),
        ModeBindings {
            keys,
            removed: BTreeSet::new(),
        },
    );
    let outcome = runtime.reload_keybindings(PartialKeybindingsConfig {
        modes: Some(modes),
        ..PartialKeybindingsConfig::default()
    });
    assert_eq!(
        outcome.report.verdict(),
        KeymapVerdict::Apply,
        "test setup: the candidate binding must apply cleanly"
    );
}

#[test]
fn a_key_from_an_unknown_client_writes_nothing() {
    let (mut runtime, fake, _client) = runtime();
    let pane = only_pane(&runtime);

    runtime.handle_key_input(ClientId::new(), chord(ModFlags::NONE, 'x'), Instant::now());

    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn a_key_writes_nothing_when_the_client_has_no_focused_pane() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let tab = runtime
        .session_for_client(client)
        .expect("session")
        .clients
        .get(client)
        .expect("client")
        .active_tab();
    runtime
        .session_for_client_mut(client)
        .expect("session")
        .clients
        .get_mut(client)
        .expect("client")
        .remove_focused_pane(tab);

    runtime.handle_key_input(client, chord(ModFlags::NONE, 'x'), Instant::now());

    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

/// A pane the tab has no room to draw takes no keystroke: the client cannot see
/// it, so a key aimed at the screen is not aimed at it. The terminal shrinks
/// below the pane's minimum, the pane is suppressed, and `l` reaches no shell.
#[test]
fn a_key_writes_nothing_when_the_focused_pane_is_suppressed() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);

    // Shrink the terminal until the sole pane no longer fits: a pane needs
    // MIN_PANE_SIZE plus its one-cell border, and 3x3 leaves less than that.
    runtime.handle_client_resize(client, Size { cols: 3, rows: 3 });
    assert!(
        runtime
            .build_snapshot(client)
            .expect("snapshot")
            .session
            .active_tab
            .all_suppressed,
        "test setup: the sole pane must be suppressed at this size"
    );

    runtime.handle_key_input(client, chord(ModFlags::NONE, 'l'), Instant::now());

    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

/// A key still reaches the pane once the terminal grows back: suppression
/// blocks the write while it lasts, and leaves nothing latched behind it.
#[test]
fn a_key_reaches_the_pane_again_once_it_is_no_longer_suppressed() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);

    runtime.handle_client_resize(client, Size { cols: 3, rows: 3 });
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'l'), Instant::now());
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());

    runtime.handle_client_resize(client, Size { cols: 80, rows: 24 });
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'l'), Instant::now());

    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![b'l']]);
}

/// A plugin pane has no PTY behind it, so the bytes a chord encodes are not
/// its to read — even though it is focused, on screen, and its id has a live
/// PTY handle in the backend from when it was a terminal pane.
#[test]
fn a_key_writes_nothing_when_the_focused_pane_is_a_plugin_pane() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);

    // Re-file the focused pane's record under `Plugin`, keeping its id: the
    // layout leaf, the focus, and the PTY handle all stay exactly as they were,
    // so only the pane's KIND can explain a missing write.
    let session_id = runtime.session_for_client(client).expect("session").id;
    let session = runtime.sessions.get_mut(&session_id).expect("session");
    let created_at = session.panes.get(pane).expect("pane record").created_at;
    session.panes.remove(pane);
    session
        .panes
        .insert(PaneRecord::new_with_kind(
            pane,
            PaneKind::Plugin {
                plugin_id: PluginId::new(),
            },
            created_at,
        ))
        .expect("re-inserting a removed pane id");

    runtime.handle_key_input(client, chord(ModFlags::NONE, 'l'), Instant::now());

    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

/// Zoom is per-client, so one client zooming a pane does not silence another
/// client's keys. A zooms its pane; B, tiled on the same tab, keeps typing into
/// the pane B can still see.
///
/// The guard asks "does the layout draw this pane FOR THIS CLIENT" — if it asked
/// the tab instead, B's pane would look hidden behind A's zoom and B's keystrokes
/// would vanish.
#[test]
fn one_clients_zoom_does_not_stop_another_clients_keys() {
    let (mut runtime, fake, client_a) = runtime();
    let now = Instant::now();
    let first = only_pane(&runtime);

    // Split so the tab has two panes; client A's focus lands on the new one.
    runtime.handle_key_input(client_a, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client_a, chord(ModFlags::NONE, 'n'), now);
    let second = focused_pane(&runtime, client_a);
    assert_ne!(second, first);

    // Client B joins the tab, focused on the first pane.
    let (session_id, tab_id) = {
        let session = runtime.session_for_client(client_a).expect("session");
        (
            session.id,
            session.clients.get(client_a).expect("client").active_tab(),
        )
    };
    let client_b = ClientId::new();
    let mut joining = Client::new(
        client_b,
        session_id,
        SystemTime::now(),
        Size { cols: 80, rows: 24 },
        tab_id,
    );
    joining.update_focused_pane(tab_id, first);
    runtime
        .sessions
        .get_mut(&session_id)
        .expect("session")
        .attach_client(joining);

    // Client A zooms its own pane, hiding `first` — from A's view only.
    dispatch(&mut runtime, client_a, Command::TogglePaneFullscreen);

    // B types. B is tiled and can see `first`, so its key lands there.
    runtime.handle_key_input(client_b, chord(ModFlags::NONE, 'y'), now);
    assert_eq!(fake.writes(first).expect("writes"), vec![vec![b'y']]);

    // A types. A can see its zoomed pane, so its key lands there.
    runtime.handle_key_input(client_a, chord(ModFlags::NONE, 'z'), now);
    assert_eq!(fake.writes(second).expect("writes"), vec![vec![b'z']]);
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
    let (mut runtime, fake, client_a) = runtime();
    let now = Instant::now();
    let first = only_pane(&runtime);

    // Stack a second pane onto the first. The new member becomes the active one
    // and takes client A's focus; `first` collapses to a header.
    dispatch(
        &mut runtime,
        client_a,
        Command::NewPane(NewPaneArgs {
            source: Some(first),
            direction: None,
            stacked: true,
            cwd: None,
            command: None,
            client: Some(client_a),
        }),
    );
    let second = focused_pane(&runtime, client_a);
    assert_ne!(second, first, "test setup: the stacked pane took focus");

    // Client B joins the same tab.
    let (session_id, tab_id) = {
        let session = runtime.session_for_client(client_a).expect("session");
        (
            session.id,
            session.clients.get(client_a).expect("client").active_tab(),
        )
    };
    let client_b = ClientId::new();
    let mut joining = Client::new(
        client_b,
        session_id,
        SystemTime::now(),
        Size { cols: 80, rows: 24 },
        tab_id,
    );
    joining.update_focused_pane(tab_id, second);
    runtime
        .sessions
        .get_mut(&session_id)
        .expect("session")
        .attach_client(joining);

    // Client B focuses the other member, which activates it — so `second`, the
    // pane client A still has focused, collapses to a header.
    dispatch(
        &mut runtime,
        client_b,
        Command::FocusPane(FocusPaneArgs {
            target: FocusTarget::Pane(first),
            client: Some(client_b),
        }),
    );
    assert_eq!(
        focused_pane(&runtime, client_a),
        second,
        "test setup: client A's focus did not move"
    );

    // Client A types at a pane that now draws nothing; client B types at the
    // member that is drawn.
    runtime.handle_key_input(client_a, chord(ModFlags::NONE, 'z'), now);
    runtime.handle_key_input(client_b, chord(ModFlags::NONE, 'y'), now);

    assert_eq!(fake.writes(second).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(fake.writes(first).expect("writes"), vec![vec![b'y']]);
}

#[test]
fn pending_sequences_stay_independent_across_clients_in_the_same_session() {
    let (mut runtime, fake, client_a) = runtime();
    let now = Instant::now();
    let original_pane = only_pane(&runtime);

    // Split: client A's focus moves to the new pane, leaving `original_pane`
    // unfocused by anyone yet.
    runtime.handle_key_input(client_a, chord(ModFlags::CTRL, 'p'), now);
    runtime.handle_key_input(client_a, chord(ModFlags::NONE, 'n'), now);
    let pane_a = focused_pane(&runtime, client_a);
    assert_ne!(pane_a, original_pane);

    // Client B joins the same session, focused on the original pane — a
    // different pane than client A's.
    let (session_id, tab_id) = {
        let session = runtime.session_for_client(client_a).expect("session");
        (
            session.id,
            session.clients.get(client_a).expect("client").active_tab(),
        )
    };
    let client_b = ClientId::new();
    let mut second = Client::new(
        client_b,
        session_id,
        SystemTime::now(),
        Size { cols: 80, rows: 24 },
        tab_id,
    );
    second.update_focused_pane(tab_id, original_pane);
    runtime
        .sessions
        .get_mut(&session_id)
        .expect("session")
        .attach_client(second);

    // Client A opens the pane prefix and leaves it hanging...
    runtime.handle_key_input(client_a, chord(ModFlags::CTRL, 'p'), now);
    // ...client B, meanwhile, sends an unrelated unbound key straight through
    // on its own (different) pane.
    runtime.handle_key_input(client_b, chord(ModFlags::NONE, 'z'), now);

    // Only `z` reaches client B's own pane — never client A's buffered
    // `<C-p>` byte, and client A's held pane sees nothing at all.
    assert_eq!(
        fake.writes(original_pane).expect("writes"),
        vec![vec![b'z']]
    );
    assert_eq!(fake.writes(pane_a).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(
        runtime
            .build_snapshot(client_a)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p')))
    );
    assert_eq!(
        runtime
            .build_snapshot(client_b)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
}

#[test]
fn take_pending_reads_only_the_requested_clients_own_state() {
    let (mut runtime, _fake, client_a) = runtime();
    // Give client A a real pending sequence via a normal keypress.
    runtime.handle_key_input(client_a, chord(ModFlags::CTRL, 'p'), Instant::now());

    // Client B joins the same session with no pending of its own.
    let (session_id, tab_id) = {
        let session = runtime.session_for_client(client_a).expect("session");
        (
            session.id,
            session.clients.get(client_a).expect("client").active_tab(),
        )
    };
    let client_b = ClientId::new();
    let mut second = Client::new(
        client_b,
        session_id,
        SystemTime::now(),
        Size { cols: 80, rows: 24 },
        tab_id,
    );
    second.update_focused_pane(tab_id, only_pane(&runtime));
    runtime
        .sessions
        .get_mut(&session_id)
        .expect("session")
        .attach_client(second);

    // Reading client B's pending must return B's own (empty) state, never
    // client A's — and must leave client A's pending untouched.
    let (_, pending_b) = runtime.take_pending(client_b).expect("client b resolves");
    assert_eq!(pending_b, None);
    let (_, pending_a) = runtime.take_pending(client_a).expect("client a resolves");
    assert_eq!(
        pending_a.map(|pending| pending.sequence),
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p')))
    );
}

#[test]
fn a_sequence_grows_to_the_chord_depth_cap_and_no_further() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    // A 4-chord binding, exactly the default `max_chord_depth`. The cap bounds
    // pending state without a check on the input path: a sequence only grows
    // while a longer live binding still starts with it, and the merge drops any
    // binding past the cap, so no pending sequence can outgrow it.
    let long = KeySequence::new(
        chord(ModFlags::CTRL, 'y'),
        vec![
            chord(ModFlags::NONE, 'a'),
            chord(ModFlags::NONE, 'b'),
            chord(ModFlags::NONE, 'c'),
        ],
    );
    bind_normal(
        &mut runtime,
        long.clone(),
        ActionRef::core("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    let tabs_before = runtime
        .sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    let now = Instant::now();
    for chord in long.chords() {
        runtime.handle_key_input(client, *chord, now);
    }

    // The full-depth binding fires, the sequence closes, and nothing along the
    // way was typed at the pane.
    assert_eq!(
        runtime
            .sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tabs_before + 1
    );
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn the_unlock_chord_escapes_a_locked_client_from_inside_an_open_sequence() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    // A locked-mode sequence of the user's own: `<C-x> a`. Pressing `<C-x>`
    // opens it, so the client is locked AND mid-sequence — the state the unlock
    // guarantee has to survive.
    bind_locked(
        &mut runtime,
        KeySequence::new(chord(ModFlags::CTRL, 'x'), vec![chord(ModFlags::NONE, 'a')]),
        ActionRef::core("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), now);
    assert_eq!(lock_mode(&runtime, client), LockMode::Locked);
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'x'), now);
    assert!(runtime
        .build_snapshot(client)
        .unwrap()
        .client
        .pending_sequence
        .is_some());

    // The unlock chord resolves ahead of the keymap and ahead of the open
    // sequence: the client unlocks, the held `<C-x>` is dropped rather than
    // typed at the pane, and no pending sequence survives into normal mode.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), now);
    assert_eq!(lock_mode(&runtime, client), LockMode::Normal);
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn a_locked_binding_holding_the_unlock_chord_never_fires_and_never_captures() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    // `<C-x> <C-l>` in locked mode: the unlock resolves at the `<C-l>` wherever
    // it is pressed, so this binding can never fire. The config layer knows it
    // is dead and drops it, which is what keeps the two halves honest — if the
    // merge admitted it, `<C-x>` would become a live prefix that captures the
    // keyboard and offers a hint-bar continuation that silently unlocks.
    bind_locked(
        &mut runtime,
        KeySequence::new(
            chord(ModFlags::CTRL, 'x'),
            vec![KeybindingsConfig::RESERVED_UNLOCK],
        ),
        ActionRef::core("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), now);
    assert_eq!(lock_mode(&runtime, client), LockMode::Locked);
    let tabs_before = runtime
        .sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    // The dead binding wins no key: `<C-x>` opens no sequence and passes to the
    // pane verbatim, exactly as locked mode passes every unbound key.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'x'), now);
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![0x18]]);

    // And the unlock still unlocks — it never became a continuation of anything.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), now);
    assert_eq!(lock_mode(&runtime, client), LockMode::Normal);
    assert_eq!(
        runtime
            .sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tabs_before,
        "the dead binding's action must never run"
    );
    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![0x18]]);
}

#[test]
fn the_unlock_chord_escapes_even_when_the_locked_keymap_lost_its_unlock_binding() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), now);
    assert_eq!(lock_mode(&runtime, client), LockMode::Locked);

    // Strip locked mode's bindings out of the resolved keymap entirely — the
    // shape a keymap layer that shadowed or removed the unlock entry would
    // leave. The escape does not read the keymap, so it still fires.
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new("locked"),
        ModeBindings {
            keys: BTreeMap::new(),
            removed: BTreeSet::new(),
        },
    );
    runtime.keymap_hints = KeymapHintCatalog::from_parts(
        &[KeyMapLayer {
            origin: LayerOrigin::Defaults,
            modes,
        }],
        &KeybindingsConfig::default(),
        &runtime.action_registry,
    );

    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), now);
    assert_eq!(lock_mode(&runtime, client), LockMode::Normal);
}

#[test]
fn expire_key_sequences_before_the_deadline_leaves_pending_intact() {
    let (mut runtime, _fake, client) = runtime();
    // `<C-y>` alone is both a complete binding and a prefix of `<C-y> x`, so
    // pressing it arms an ambiguity deadline.
    bind_normal_all(
        &mut runtime,
        vec![
            (
                KeySequence::new(chord(ModFlags::CTRL, 'y'), Vec::new()),
                ActionRef::core("new-tab").expect("valid core action name"),
                ActionArgs::None,
            ),
            (
                KeySequence::new(chord(ModFlags::CTRL, 'y'), vec![chord(ModFlags::NONE, 'x')]),
                ActionRef::core("unlock").expect("valid core action name"),
                ActionArgs::None,
            ),
        ],
    );
    let now = Instant::now();
    let tabs_before = runtime
        .sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'y'), now);
    let deadline = now + runtime.keymap_hints.chord_timeout();
    runtime.expire_key_sequences(deadline - Duration::from_millis(1));

    assert_eq!(
        runtime
            .sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tabs_before
    );
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(chord(ModFlags::CTRL, 'y')))
    );
}

#[test]
fn expire_key_sequences_at_the_deadline_fires_the_ambiguous_bindings_exact_match() {
    let (mut runtime, _fake, client) = runtime();
    bind_normal_all(
        &mut runtime,
        vec![
            (
                KeySequence::new(chord(ModFlags::CTRL, 'y'), Vec::new()),
                ActionRef::core("new-tab").expect("valid core action name"),
                ActionArgs::None,
            ),
            (
                KeySequence::new(chord(ModFlags::CTRL, 'y'), vec![chord(ModFlags::NONE, 'x')]),
                ActionRef::core("unlock").expect("valid core action name"),
                ActionArgs::None,
            ),
        ],
    );
    let now = Instant::now();
    let tabs_before = runtime
        .sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'y'), now);
    let deadline = now + runtime.keymap_hints.chord_timeout();
    runtime.expire_key_sequences(deadline);

    assert_eq!(
        runtime
            .sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tabs_before + 1
    );
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
}

#[test]
fn a_held_exact_binding_survives_a_key_it_cannot_use_and_fires_at_its_deadline() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    // `<C-y>` alone is both a complete binding and a prefix of `<C-y> x`, so
    // pressing it opens a sequence that carries an ambiguity deadline.
    bind_normal_all(
        &mut runtime,
        vec![
            (
                KeySequence::new(chord(ModFlags::CTRL, 'y'), Vec::new()),
                ActionRef::core("new-tab").expect("valid core action name"),
                ActionArgs::None,
            ),
            (
                KeySequence::new(chord(ModFlags::CTRL, 'y'), vec![chord(ModFlags::NONE, 'x')]),
                ActionRef::core("unlock").expect("valid core action name"),
                ActionArgs::None,
            ),
        ],
    );
    let now = Instant::now();
    let tabs_before = runtime
        .sessions()
        .values()
        .next()
        .expect("session")
        .tabs
        .len();

    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'y'), now);
    // `z` extends `<C-y>` into nothing, so it is discarded — the sequence is not
    // abandoned by a key it cannot use, and its deadline still stands.
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'z'), now);
    assert_eq!(
        runtime
            .sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tabs_before,
        "the held binding waits for its deadline, not for a mismatch"
    );
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(chord(ModFlags::CTRL, 'y')))
    );

    // The deadline decides: `<C-y>`'s own binding fires, and the client lands on
    // the new tab. Neither the held chord nor the discarded `z` was ever typed.
    let deadline = now + runtime.keymap_hints.chord_timeout();
    runtime.expire_key_sequences(deadline);
    assert_eq!(
        runtime
            .sessions()
            .values()
            .next()
            .expect("session")
            .tabs
            .len(),
        tabs_before + 1
    );
    let new_pane = focused_pane(&runtime, client);
    assert_ne!(
        new_pane, pane,
        "new-tab must have switched focus to a new pane"
    );
    assert_eq!(
        fake.writes(new_pane).expect("writes"),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
}

#[test]
fn typing_snaps_a_scrolled_up_view_back_to_live_output() {
    let (mut runtime, pane, client) = runtime_scrolled_up();
    assert_eq!(scroll_offset(&runtime, client, pane), 3);

    runtime.handle_key_input(client, chord(ModFlags::NONE, 'a'), Instant::now());
    assert_eq!(scroll_offset(&runtime, client, pane), 0);
}

#[test]
fn typing_leaves_the_view_parked_when_scroll_on_input_is_off() {
    let (mut runtime, pane, client) = runtime_scrolled_up();
    runtime.config.scrollback.scroll_on_input = false;

    runtime.handle_key_input(client, chord(ModFlags::NONE, 'a'), Instant::now());
    assert_eq!(scroll_offset(&runtime, client, pane), 3);
}

#[test]
fn typing_on_the_alternate_screen_leaves_the_view_to_the_program() {
    let (mut runtime, pane, client) = runtime_scrolled_up();
    runtime.handle_pty_output(pane, b"\x1b[?1049h"); // enter the alternate screen

    runtime.handle_key_input(client, chord(ModFlags::NONE, 'a'), Instant::now());
    assert_eq!(scroll_offset(&runtime, client, pane), 3);
}

#[test]
fn pasting_snaps_a_scrolled_up_view_back_to_live_output() {
    let (mut runtime, pane, client) = runtime_scrolled_up();

    runtime.handle_host_paste(client, "ls\n");
    assert_eq!(scroll_offset(&runtime, client, pane), 0);
}
