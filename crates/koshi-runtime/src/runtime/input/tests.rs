//! End-to-end default-keymap tests: passthrough, lock escape, prefix display,
//! multi-chord dispatch, timeout fallback, open-sequence capture, and pane
//! resize.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc};

use koshi_config::conflict::KeymapVerdict;
use koshi_config::layer::{PartialKeybindingsConfig, PartialKoshiConfig, PartialLayoutDefaults};
use koshi_config::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::action::ActionRef;
use koshi_core::command::{Command, CommandResult, FocusPaneArgs, FocusTarget, NewPaneArgs};
use koshi_core::geometry::{Direction, Size};
use koshi_core::ids::{PluginId, SessionId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags, NamedKey};
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

use crate::placeholder::{NullSnapshotProvider, NullStorage};
use crate::runtime::bus::EventFilter;
use crate::server::Server;

fn runtime() -> (Server, Arc<FakePtyBackend>, ClientId, ViewerClient) {
    let fake = Arc::new(FakePtyBackend::new());
    let (tx, rx) = mpsc::channel();
    let mut runtime = Server::new(
        fake.clone(),
        Arc::new(NullSnapshotProvider),
        Arc::new(NullStorage),
        rx,
        tx,
    );
    let client = runtime
        .bootstrap_local(
            SessionId::new(),
            Size { cols: 80, rows: 24 },
            SystemTime::UNIX_EPOCH,
        )
        .expect("bootstrap");
    let viewer = viewer_for(&mut runtime, client);
    (runtime, fake, client, viewer)
}

/// The viewer half for `client_id`: it holds the keymap and resolves every
/// press below before the session hears about it.
fn viewer_for(runtime: &mut Server, client_id: ClientId) -> ViewerClient {
    ViewerClient::new(
        client_id,
        Size { cols: 80, rows: 24 },
        runtime.subscribe(client_id, EventFilter::All),
        TerminalCleanupGuard::new(),
    )
}

/// One keypress, the way the running binary delivers it: the viewer decides
/// what the chord means, and only what it resolves to reaches the session.
fn press(runtime: &mut Server, viewer: &mut ViewerClient, chord: KeyChord, now: Instant) {
    let client_id = viewer.id();
    match viewer.resolve_key(chord, now) {
        KeyOutcome::Fire(bound) => {
            let direction = viewer.config().layout.new_pane_direction;
            runtime.handle_bound_action(client_id, bound, direction);
        }
        KeyOutcome::PassThrough(chord) => runtime.handle_key_press(client_id, chord),
        KeyOutcome::Pending | KeyOutcome::Discard => {}
    }
    viewer.apply_events();
}

/// Fire an open sequence's binding if its ambiguity deadline has passed.
fn expire(runtime: &mut Server, viewer: &mut ViewerClient, now: Instant) {
    if let Some(bound) = viewer.expire_key_sequence(now) {
        let direction = viewer.config().layout.new_pane_direction;
        runtime.handle_bound_action(viewer.id(), bound, direction);
    }
    viewer.apply_events();
}

fn chord(mods: ModFlags, key: char) -> KeyChord {
    KeyChord::new(mods, Key::Char(key))
}

/// An unmodified named key, for the arrows the default focus and resize
/// sequences continue with.
fn named(key: NamedKey) -> KeyChord {
    KeyChord::new(ModFlags::NONE, Key::Named(key))
}

fn only_pane(runtime: &Server) -> koshi_core::ids::PaneId {
    *runtime.pty_handles.keys().next().expect("one pane")
}

/// The client's scroll offset for the pane — `0` means the view follows live output.
fn scroll_offset(runtime: &Server, client: ClientId, pane: koshi_core::ids::PaneId) -> usize {
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
fn runtime_scrolled_up() -> (Server, koshi_core::ids::PaneId, ClientId, ViewerClient) {
    let (mut runtime, _fake, client, viewer) = runtime();
    let pane = only_pane(&runtime);
    runtime.handle_pty_output(pane, &b"\n".repeat(40)); // push lines into history
    runtime.scroll_up(client, pane, 3);
    (runtime, pane, client, viewer)
}

/// Run `command` as if `client` had issued it from a keybinding, asserting it
/// was applied — a test that silently dispatched a rejected command would be
/// asserting against a state it never reached.
fn dispatch(runtime: &mut Server, client: ClientId, command: Command) {
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
    let (mut runtime, fake, _client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![b'a']]);
}

#[test]
fn an_unbound_arrow_follows_the_focused_panes_application_cursor_mode() {
    let (mut runtime, fake, _client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    let up = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Up));

    // A shell leaves application-cursor-keys mode off, and reads `ESC [ A`.
    press(&mut runtime, &mut viewer, up, Instant::now());
    assert_eq!(fake.writes(pane).expect("writes"), vec![b"\x1b[A".to_vec()]);

    // vim turns it on (DECCKM, `ESC [ ? 1 h`) and now reads `ESC O A` for the
    // same press. The pane's mode, not the press, picks the bytes.
    runtime.handle_pty_output(pane, b"\x1b[?1h");
    press(&mut runtime, &mut viewer, up, Instant::now());
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
    let (mut runtime, fake, client, mut viewer) = runtime();
    let first = only_pane(&runtime);
    let up = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Up));
    let now = Instant::now();

    // A second pane, which takes focus. It runs vim: application-cursor-keys on.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
    let second = focused_pane(&runtime, client);
    assert_ne!(second, first);
    runtime.handle_pty_output(second, b"\x1b[?1h");

    // `<Up> x` makes a bare `<Up>` a prefix, so pressing it opens a sequence.
    bind_normal(
        &mut viewer,
        KeySequence::new(up, vec![chord(ModFlags::NONE, 'x')]),
        ActionRef::core("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    press(&mut runtime, &mut viewer, up, now);

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
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'z'), now);
    assert_eq!(fake.writes(second).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(fake.writes(first).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(up)),
        "the open sequence outlives a key it cannot use"
    );

    // Escape leaves the sequence, and still nothing is typed at either pane.
    press(
        &mut runtime,
        &mut viewer,
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
    let (mut runtime, fake, _client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    let up = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Up));

    // `<Up> x` makes a bare `<Up>` a prefix, so pressing it opens a sequence
    // instead of passing straight through.
    bind_normal(
        &mut viewer,
        KeySequence::new(up, vec![chord(ModFlags::NONE, 'x')]),
        ActionRef::core("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );

    // Press `<Up>` while the pane is a plain shell: buffered, nothing written.
    let now = Instant::now();
    press(&mut runtime, &mut viewer, up, now);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());

    // The pane now turns application-cursor-keys mode ON, mid-sequence.
    runtime.handle_pty_output(pane, b"\x1b[?1h");

    // `z` continues nothing, so it is discarded and the sequence stands. The
    // pane sees neither the arrow nor the `z`, in either cursor mode.
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'z'), now);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());

    // Completing the sequence fires the binding — still no bytes to the pane.
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'x'), now);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn a_modified_arrow_keeps_its_modifier_on_the_way_to_the_pane() {
    let (mut runtime, fake, _client, mut viewer) = runtime();
    let pane = only_pane(&runtime);

    // `<C-Right>` is a word-jump to a shell; dropping the Control would leave
    // it a plain Right and move one character instead.
    press(
        &mut runtime,
        &mut viewer,
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
    let (mut runtime, fake, client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    // `<C-l>` locks in normal mode…
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'l'), now);
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
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'l'), now);
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
    let (mut runtime, fake, _client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::CTRL, 'q'),
        Instant::now(),
    );
    assert!(runtime.quit_requested());
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn quit_binding_fires_in_locked_mode_too() {
    let (mut runtime, fake, _client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'l'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'q'), now);
    assert!(runtime.quit_requested());
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn continuous_resize_keeps_the_prefix_armed_for_repeat_presses() {
    let (mut runtime, _fake, _client, mut viewer) = runtime();
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);

    // First resize: full `<C-s> <Left>` sequence.
    let sizes_start = runtime.pty_sizes.clone();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 's'), now);
    press(&mut runtime, &mut viewer, named(NamedKey::Left), now);
    let sizes_once = runtime.pty_sizes.clone();
    assert_ne!(sizes_once, sizes_start);

    // The prefix stayed armed: `<Left>` alone fires the resize again…
    assert_eq!(
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(chord(ModFlags::CTRL, 's')))
    );
    press(&mut runtime, &mut viewer, named(NamedKey::Left), now);
    assert_ne!(runtime.pty_sizes, sizes_once);

    // …and Escape puts the bar back to idle.
    press(
        &mut runtime,
        &mut viewer,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    assert_eq!(viewer.pending_sequence().cloned(), None);
}

#[test]
fn one_shot_bindings_clear_the_whole_sequence_after_firing() {
    let (mut runtime, _fake, _client, mut viewer) = runtime();
    let now = Instant::now();
    // `new-pane` is not continuous: after `<C-p> n` fires, nothing pends.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
    assert_eq!(runtime.pty_handles.len(), 2);
    assert_eq!(viewer.pending_sequence().cloned(), None);
}

#[test]
fn locked_mode_passes_non_unlock_keys_verbatim() {
    let (mut runtime, fake, _client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'l'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'x'), now);
    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![b'x']]);
}

#[test]
fn pane_prefix_updates_snapshot_then_new_pane_fires() {
    let (mut runtime, _fake, _client, mut viewer) = runtime();
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    assert_eq!(
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p')))
    );
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
    assert_eq!(runtime.pty_handles.len(), 2);
    assert_eq!(viewer.pending_sequence().cloned(), None);
}

#[test]
fn prefix_pending_never_expires() {
    let (mut runtime, fake, _client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    // A prefix-only sequence arms no deadline and outlives any wait: the
    // continuation hints stay up until the user presses another key.
    assert_eq!(viewer.next_key_wakeup(now), None);
    expire(&mut runtime, &mut viewer, now + Duration::from_secs(3600));
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
    assert!(viewer.pending_sequence().cloned().is_some());
}

#[test]
fn escape_cancels_a_pending_sequence_silently() {
    let (mut runtime, fake, _client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(
        &mut runtime,
        &mut viewer,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    // Neither the buffered prefix nor the Escape reaches the pane, and the
    // pending sequence is gone — the bar returns to its idle hints.
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(viewer.pending_sequence().cloned(), None);
}

#[test]
fn an_unmatched_continuation_is_discarded_and_the_sequence_stands() {
    let (mut runtime, fake, _client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    // `<C-p>` opens the pane prefix. `z` binds nothing under it: it goes
    // nowhere, and the prefix is still open — the shell must not see `Ctrl-P`
    // (history-back) or the `z`, because both were typed at Koshi.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'z'), now);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p')))
    );

    // The sequence is live, not merely remembered: `n` still completes it.
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
    assert_eq!(runtime.pty_handles.len(), 2);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn directional_focus_binding_moves_focus_across_a_split() {
    let (mut runtime, _fake, client, mut viewer) = runtime();
    let now = Instant::now();
    // Split: the new right pane takes focus.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
    let focused_after_split = focused_pane(&runtime, client);

    // `<C-p> <Left>` focuses the left neighbor.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, named(NamedKey::Left), now);
    let focused_left = focused_pane(&runtime, client);
    assert_ne!(focused_left, focused_after_split);

    // Focus is continuous, so the prefix stays armed: `<Right>` alone returns
    // to the right pane.
    press(&mut runtime, &mut viewer, named(NamedKey::Right), now);
    assert_eq!(focused_pane(&runtime, client), focused_after_split);
}

#[test]
fn directional_new_pane_binding_splits_on_that_side() {
    let (mut runtime, _fake, client, mut viewer) = runtime();
    let original = only_pane(&runtime);
    let now = Instant::now();

    // `<C-p> h` opens a new pane on the LEFT of the focused one, and the new
    // pane takes focus.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'h'), now);
    assert_eq!(runtime.pty_handles.len(), 2);
    let new_pane = focused_pane(&runtime, client);
    assert_ne!(new_pane, original);

    // The original pane is the new pane's RIGHT neighbor — exactly where a
    // left split puts it. A wrong split side leaves nothing to the right and
    // this focus move would stay put.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, named(NamedKey::Right), now);
    assert_eq!(focused_pane(&runtime, client), original);
}

#[test]
fn the_viewers_configured_split_direction_reaches_the_new_pane() {
    let (mut runtime, _fake, client, mut viewer) = runtime();
    let original = only_pane(&runtime);
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
    assert_eq!(viewer.config().layout.new_pane_direction, Direction::Down);

    let tab = tab_of(&runtime, client);
    let before = runtime.session_for_client(client).expect("session").tabs[&tab]
        .layout()
        .clone();

    // `<C-p> n` is the direction-less new-pane binding.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);

    let new_pane = focused_pane(&runtime, client);
    assert_ne!(new_pane, original);
    let expected =
        split_leaf(&before, original, new_pane, Direction::Down).expect("split on the source leaf");
    assert_eq!(
        runtime.session_for_client(client).expect("session").tabs[&tab].layout(),
        &expected
    );
}

#[test]
fn a_user_bound_stacked_new_pane_key_builds_a_stack() {
    let (mut runtime, _fake, client, mut viewer) = runtime();
    let original = only_pane(&runtime);
    let now = Instant::now();

    // `new-pane-stacked` ships with no default key; a user binds their own.
    bind_normal(
        &mut viewer,
        KeySequence::from(chord(ModFlags::ALT, 's')),
        ActionRef::core("new-pane-stacked").expect("valid name"),
        ActionArgs::None,
    );
    press(&mut runtime, &mut viewer, chord(ModFlags::ALT, 's'), now);

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
    let (mut runtime, _fake, client, mut viewer) = runtime();
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);

    press(&mut runtime, &mut viewer, chord(ModFlags::ALT, 'f'), now);
    let snap = runtime.build_snapshot(client).expect("snapshot");
    assert_eq!(
        snap.session.active_tab.layout_mode,
        koshi_layout::mode::LayoutMode::Fullscreen {
            focused: focused_pane(&runtime, client)
        }
    );

    press(&mut runtime, &mut viewer, chord(ModFlags::ALT, 'f'), now);
    let snap = runtime.build_snapshot(client).expect("snapshot");
    assert_eq!(
        snap.session.active_tab.layout_mode,
        koshi_layout::mode::LayoutMode::Tiled
    );
}

fn focused_pane(runtime: &Server, client: ClientId) -> koshi_core::ids::PaneId {
    let session = runtime.session_for_client(client).expect("session");
    let state = session.clients.get(client).expect("client");
    state
        .focused_pane(state.active_tab())
        .expect("a focused pane")
}

/// The tab the client is looking at.
fn tab_of(runtime: &Server, client: ClientId) -> koshi_core::ids::TabId {
    runtime
        .session_for_client(client)
        .expect("session")
        .clients
        .get(client)
        .expect("client")
        .active_tab()
}

#[test]
fn resize_prefix_moves_a_live_split_border() {
    let (mut runtime, _fake, _client, mut viewer) = runtime();
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
    let sizes_before = runtime.pty_sizes.clone();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 's'), now);
    press(&mut runtime, &mut viewer, named(NamedKey::Left), now);
    assert_ne!(runtime.pty_sizes, sizes_before);
}

#[test]
fn continuous_focus_rearm_walks_panes_with_repeated_arrows() {
    let (mut runtime, _fake, client, mut viewer) = runtime();
    let now = Instant::now();
    // Two splits: three panes across, focus on the right-most.
    for _ in 0..2 {
        press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
        press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
    }
    let rightmost = focused_pane(&runtime, client);

    // `<C-p> ←` moves one pane left and re-arms the prefix…
    let left = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Left));
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, left, now);
    let middle = focused_pane(&runtime, client);
    assert_ne!(middle, rightmost);
    assert_eq!(
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p')))
    );

    // …so a bare ← walks one further pane left.
    press(&mut runtime, &mut viewer, left, now);
    let leftmost = focused_pane(&runtime, client);
    assert_ne!(leftmost, middle);
    assert_ne!(leftmost, rightmost);
}

#[test]
fn abandoned_rearmed_prefix_writes_nothing_to_the_pane() {
    let (mut runtime, fake, client, mut viewer) = runtime();
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
    let focused = focused_pane(&runtime, client);

    // Resize once, leave the re-armed prefix hanging, then cancel with Esc:
    // the re-armed prefix carries no fallback bytes, so the shell sees none.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 's'), now);
    press(&mut runtime, &mut viewer, named(NamedKey::Left), now);
    press(
        &mut runtime,
        &mut viewer,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    assert_eq!(fake.writes(focused).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(viewer.pending_sequence().cloned(), None);
}

#[test]
fn an_unmatched_key_under_a_rearmed_prefix_is_discarded_and_it_stays_armed() {
    let (mut runtime, fake, client, mut viewer) = runtime();
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
    let focused = focused_pane(&runtime, client);

    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 's'), now);
    press(&mut runtime, &mut viewer, named(NamedKey::Left), now);
    let sizes_after_one_resize = runtime.pty_sizes.clone();

    // A re-armed prefix is an open sequence like any other, and captures like
    // one: `z` resizes nothing, so it is discarded — not passed to the shell —
    // and `<C-s>` stays armed.
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'z'), now);
    assert_eq!(fake.writes(focused).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(runtime.pty_sizes, sizes_after_one_resize);
    assert_eq!(
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(chord(ModFlags::CTRL, 's')))
    );

    // Still armed, so the next `<Left>` resizes again without re-pressing `<C-s>`.
    press(&mut runtime, &mut viewer, named(NamedKey::Left), now);
    assert_ne!(runtime.pty_sizes, sizes_after_one_resize);

    // Escape is the way out, and it types nothing at the pane.
    press(
        &mut runtime,
        &mut viewer,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        now,
    );
    assert_eq!(fake.writes(focused).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(viewer.pending_sequence().cloned(), None);
}

#[test]
fn resize_binding_at_the_tab_edge_moves_the_opposite_border() {
    let (mut runtime, _fake, client, mut viewer) = runtime();
    let now = Instant::now();
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
    let focused = focused_pane(&runtime, client);
    let before = runtime.pty_sizes[&focused];

    // The focused pane touches the tab's right edge: `<C-s> l` has no right
    // border to grow through, so its left border moves right — it shrinks.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 's'), now);
    press(&mut runtime, &mut viewer, named(NamedKey::Right), now);
    let after = runtime.pty_sizes[&focused];
    assert_eq!(after.cols, before.cols - 1);
    assert_eq!(after.rows, before.rows);
}

/// Bind one `normal`-mode sequence to `action` in the viewer's own keymap.
fn bind_normal(
    viewer: &mut ViewerClient,
    sequence: KeySequence,
    action: ActionRef,
    args: ActionArgs,
) {
    bind_normal_all(viewer, vec![(sequence, action, args)]);
}

/// Bind one `locked`-mode sequence to `action`, keeping the shipped locked
/// bindings (the unlock chord among them) beside it — a user layer that dropped
/// the unlock entry would be refused by conflict detection.
fn bind_locked(
    viewer: &mut ViewerClient,
    sequence: KeySequence,
    action: ActionRef,
    args: ActionArgs,
) {
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
    let report = viewer.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            modes: Some(modes),
            ..PartialKeybindingsConfig::default()
        }),
    );
    assert_eq!(
        report.expect("a keybinding file was given").verdict(),
        KeymapVerdict::Apply,
        "test setup: the candidate binding must apply cleanly"
    );
}

/// How long the viewer waits for the next chord of an ambiguous sequence.
fn chord_timeout(viewer: &ViewerClient) -> Duration {
    Duration::from_millis(u64::from(viewer.config().keybindings.chord_timeout_ms))
}

/// The client's current lock mode.
fn lock_mode(runtime: &Server, client: ClientId) -> LockMode {
    runtime
        .session_for_client(client)
        .expect("session")
        .clients
        .get(client)
        .expect("client")
        .lock_mode()
}

/// Bind every `(sequence, action, args)` triple in `bindings` under `normal`
/// mode in one `keybinding.kdl` the viewer reads. Reading the file replaces
/// the whole keybinding layer, so binding several sequences needs one call
/// with every entry, not several calls that would each overwrite the last.
fn bind_normal_all(viewer: &mut ViewerClient, bindings: Vec<(KeySequence, ActionRef, ActionArgs)>) {
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
    let report = viewer.load_startup_config(
        None,
        None,
        Some(PartialKeybindingsConfig {
            modes: Some(modes),
            ..PartialKeybindingsConfig::default()
        }),
    );
    assert_eq!(
        report.expect("a keybinding file was given").verdict(),
        KeymapVerdict::Apply,
        "test setup: the candidate binding must apply cleanly"
    );
}

#[test]
fn a_key_from_an_unknown_client_writes_nothing() {
    let (mut runtime, fake, _client, _viewer) = runtime();
    let pane = only_pane(&runtime);

    // A press arriving for a client the session does not know resolves to no
    // pane, so nothing is written rather than landing on someone else's.
    runtime.handle_key_press(ClientId::new(), chord(ModFlags::NONE, 'x'));

    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn a_key_writes_nothing_when_the_client_has_no_focused_pane() {
    let (mut runtime, fake, client, mut viewer) = runtime();
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

    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::NONE, 'x'),
        Instant::now(),
    );

    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

/// A pane the tab has no room to draw takes no keystroke: the client cannot see
/// it, so a key aimed at the screen is not aimed at it. The terminal shrinks
/// below the pane's minimum, the pane is suppressed, and `l` reaches no shell.
#[test]
fn a_key_writes_nothing_when_the_focused_pane_is_suppressed() {
    let (mut runtime, fake, client, mut viewer) = runtime();
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

    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::NONE, 'l'),
        Instant::now(),
    );

    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

/// A key still reaches the pane once the terminal grows back: suppression
/// blocks the write while it lasts, and leaves nothing latched behind it.
#[test]
fn a_key_reaches_the_pane_again_once_it_is_no_longer_suppressed() {
    let (mut runtime, fake, client, mut viewer) = runtime();
    let pane = only_pane(&runtime);

    runtime.handle_client_resize(client, Size { cols: 3, rows: 3 });
    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::NONE, 'l'),
        Instant::now(),
    );
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());

    runtime.handle_client_resize(client, Size { cols: 80, rows: 24 });
    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::NONE, 'l'),
        Instant::now(),
    );

    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![b'l']]);
}

/// A plugin pane has no PTY behind it, so the bytes a chord encodes are not
/// its to read — even though it is focused, on screen, and its id has a live
/// PTY handle in the backend from when it was a terminal pane.
#[test]
fn a_key_writes_nothing_when_the_focused_pane_is_a_plugin_pane() {
    let (mut runtime, fake, client, mut viewer) = runtime();
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

    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::NONE, 'l'),
        Instant::now(),
    );

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
    let (mut runtime, fake, client_a, mut viewer) = runtime();
    let now = Instant::now();
    let first = only_pane(&runtime);

    // Split so the tab has two panes; client A's focus lands on the new one.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
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
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    joining.update_focused_pane(tab_id, first);
    runtime
        .sessions
        .get_mut(&session_id)
        .expect("session")
        .attach_client(joining);

    let mut viewer_b = viewer_for(&mut runtime, client_b);

    // Client A zooms its own pane, hiding `first` — from A's view only.
    dispatch(&mut runtime, client_a, Command::TogglePaneFullscreen);

    // B types. B is tiled and can see `first`, so its key lands there.
    press(&mut runtime, &mut viewer_b, chord(ModFlags::NONE, 'y'), now);
    assert_eq!(fake.writes(first).expect("writes"), vec![vec![b'y']]);

    // A types. A can see its zoomed pane, so its key lands there.
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'z'), now);
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
    let (mut runtime, fake, client_a, mut viewer) = runtime();
    let now = Instant::now();
    let first = only_pane(&runtime);

    // Stack a second pane onto the first. The new member becomes the active one
    // and takes client A's focus; `first` collapses to a header.
    dispatch(
        &mut runtime,
        client_a,
        Command::NewPane(NewPaneArgs {
            source: Some(first),
            tab: None,
            direction: Direction::Right,
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
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
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
    let mut viewer_b = viewer_for(&mut runtime, client_b);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'z'), now);
    press(&mut runtime, &mut viewer_b, chord(ModFlags::NONE, 'y'), now);

    assert_eq!(fake.writes(second).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(fake.writes(first).expect("writes"), vec![vec![b'y']]);
}

#[test]
fn pending_sequences_stay_independent_across_clients_in_the_same_session() {
    let (mut runtime, fake, client_a, mut viewer) = runtime();
    let now = Instant::now();
    let original_pane = only_pane(&runtime);

    // Split: client A's focus moves to the new pane, leaving `original_pane`
    // unfocused by anyone yet.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'n'), now);
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
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    second.update_focused_pane(tab_id, original_pane);
    runtime
        .sessions
        .get_mut(&session_id)
        .expect("session")
        .attach_client(second);

    // Each viewer holds its own keymap and its own open sequence, so client B
    // gets one of its own.
    let mut viewer_b = viewer_for(&mut runtime, client_b);

    // Client A opens the pane prefix and leaves it hanging...
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'p'), now);
    // ...client B, meanwhile, sends an unrelated unbound key straight through
    // on its own (different) pane.
    press(&mut runtime, &mut viewer_b, chord(ModFlags::NONE, 'z'), now);

    // Only `z` reaches client B's own pane — never client A's buffered
    // `<C-p>` byte, and client A's held pane sees nothing at all.
    assert_eq!(
        fake.writes(original_pane).expect("writes"),
        vec![vec![b'z']]
    );
    assert_eq!(fake.writes(pane_a).expect("writes"), Vec::<Vec<u8>>::new());
    assert_eq!(
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p'))),
        "client A's sequence is still open"
    );
    assert_eq!(
        viewer_b.pending_sequence().cloned(),
        None,
        "client B never opened one of its own"
    );
}

#[test]
fn one_viewers_open_sequence_is_invisible_to_another() {
    // Each viewer owns the sequence it is typing, so one holding a prefix open
    // cannot make another viewer's next key continue it.
    let (mut runtime, _fake, client_a, mut viewer) = runtime();
    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::CTRL, 'p'),
        Instant::now(),
    );

    // Client B joins the same session with no sequence of its own.
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
        ClientOrigin::Local,
        "C-test-client".to_string(),
        0,
    );
    second.update_focused_pane(tab_id, only_pane(&runtime));
    runtime
        .sessions
        .get_mut(&session_id)
        .expect("session")
        .attach_client(second);
    let mut viewer_b = viewer_for(&mut runtime, client_b);

    assert_eq!(
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p'))),
        "client A is mid-sequence"
    );
    assert_eq!(
        viewer_b.pending_sequence().cloned(),
        None,
        "client B has nothing open"
    );

    // So B's `n` is its own key, not the continuation that would fire
    // `<C-p> n` — it types, and A's sequence is still waiting.
    assert_eq!(
        viewer_b.resolve_key(chord(ModFlags::NONE, 'n'), Instant::now()),
        KeyOutcome::PassThrough(chord(ModFlags::NONE, 'n'))
    );
    assert_eq!(
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p'))),
        "A's sequence outlives B's keypress"
    );
}

#[test]
fn a_sequence_grows_to_the_chord_depth_cap_and_no_further() {
    let (mut runtime, fake, _client, mut viewer) = runtime();
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
        &mut viewer,
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
        press(&mut runtime, &mut viewer, *chord, now);
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
    assert_eq!(viewer.pending_sequence().cloned(), None);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn the_unlock_chord_escapes_a_locked_client_from_inside_an_open_sequence() {
    let (mut runtime, fake, client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    // A locked-mode sequence of the user's own: `<C-x> a`. Pressing `<C-x>`
    // opens it, so the client is locked AND mid-sequence — the state the unlock
    // guarantee has to survive.
    bind_locked(
        &mut viewer,
        KeySequence::new(chord(ModFlags::CTRL, 'x'), vec![chord(ModFlags::NONE, 'a')]),
        ActionRef::core("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'l'), now);
    assert_eq!(lock_mode(&runtime, client), LockMode::Locked);
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'x'), now);
    assert!(viewer.pending_sequence().cloned().is_some());

    // The unlock chord resolves ahead of the keymap and ahead of the open
    // sequence: the client unlocks, the held `<C-x>` is dropped rather than
    // typed at the pane, and no pending sequence survives into normal mode.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'l'), now);
    assert_eq!(lock_mode(&runtime, client), LockMode::Normal);
    assert_eq!(viewer.pending_sequence().cloned(), None);
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn a_locked_binding_holding_the_unlock_chord_never_fires_and_never_captures() {
    let (mut runtime, fake, client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    // `<C-x> <C-l>` in locked mode: the unlock resolves at the `<C-l>` wherever
    // it is pressed, so this binding can never fire. The config layer knows it
    // is dead and drops it, which is what keeps the two halves honest — if the
    // merge admitted it, `<C-x>` would become a live prefix that captures the
    // keyboard and offers a hint-bar continuation that silently unlocks.
    bind_locked(
        &mut viewer,
        KeySequence::new(
            chord(ModFlags::CTRL, 'x'),
            vec![KeybindingsConfig::RESERVED_UNLOCK],
        ),
        ActionRef::core("new-tab").expect("valid core action name"),
        ActionArgs::None,
    );
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'l'), now);
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
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'x'), now);
    assert_eq!(viewer.pending_sequence().cloned(), None);
    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![0x18]]);

    // And the unlock still unlocks — it never became a continuation of anything.
    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'l'), now);
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
fn expire_key_sequences_before_the_deadline_leaves_pending_intact() {
    let (mut runtime, _fake, _client, mut viewer) = runtime();
    // `<C-y>` alone is both a complete binding and a prefix of `<C-y> x`, so
    // pressing it arms an ambiguity deadline.
    bind_normal_all(
        &mut viewer,
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

    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'y'), now);
    let deadline = now + chord_timeout(&viewer);
    expire(
        &mut runtime,
        &mut viewer,
        deadline - Duration::from_millis(1),
    );

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
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(chord(ModFlags::CTRL, 'y')))
    );
}

#[test]
fn expire_key_sequences_at_the_deadline_fires_the_ambiguous_bindings_exact_match() {
    let (mut runtime, _fake, _client, mut viewer) = runtime();
    bind_normal_all(
        &mut viewer,
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

    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'y'), now);
    let deadline = now + chord_timeout(&viewer);
    expire(&mut runtime, &mut viewer, deadline);

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
    assert_eq!(viewer.pending_sequence().cloned(), None);
}

#[test]
fn a_held_exact_binding_survives_a_key_it_cannot_use_and_fires_at_its_deadline() {
    let (mut runtime, fake, client, mut viewer) = runtime();
    let pane = only_pane(&runtime);
    // `<C-y>` alone is both a complete binding and a prefix of `<C-y> x`, so
    // pressing it opens a sequence that carries an ambiguity deadline.
    bind_normal_all(
        &mut viewer,
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

    press(&mut runtime, &mut viewer, chord(ModFlags::CTRL, 'y'), now);
    // `z` extends `<C-y>` into nothing, so it is discarded — the sequence is not
    // abandoned by a key it cannot use, and its deadline still stands.
    press(&mut runtime, &mut viewer, chord(ModFlags::NONE, 'z'), now);
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
        viewer.pending_sequence().cloned(),
        Some(KeySequence::from(chord(ModFlags::CTRL, 'y')))
    );

    // The deadline decides: `<C-y>`'s own binding fires, and the client lands on
    // the new tab. Neither the held chord nor the discarded `z` was ever typed.
    let deadline = now + chord_timeout(&viewer);
    expire(&mut runtime, &mut viewer, deadline);
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
    assert_eq!(viewer.pending_sequence().cloned(), None);
}

#[test]
fn typing_snaps_a_scrolled_up_view_back_to_live_output() {
    let (mut runtime, pane, client, mut viewer) = runtime_scrolled_up();
    assert_eq!(scroll_offset(&runtime, client, pane), 3);

    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(scroll_offset(&runtime, client, pane), 0);
}

#[test]
fn typing_leaves_the_view_parked_when_scroll_on_input_is_off() {
    let (mut runtime, pane, client, mut viewer) = runtime_scrolled_up();
    runtime.client_config.scrollback.scroll_on_input = false;

    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(scroll_offset(&runtime, client, pane), 3);
}

#[test]
fn typing_on_the_alternate_screen_leaves_the_view_to_the_program() {
    let (mut runtime, pane, client, mut viewer) = runtime_scrolled_up();
    runtime.handle_pty_output(pane, b"\x1b[?1049h"); // enter the alternate screen

    press(
        &mut runtime,
        &mut viewer,
        chord(ModFlags::NONE, 'a'),
        Instant::now(),
    );
    assert_eq!(scroll_offset(&runtime, client, pane), 3);
}

#[test]
fn pasting_snaps_a_scrolled_up_view_back_to_live_output() {
    let (mut runtime, pane, client, _viewer) = runtime_scrolled_up();

    runtime.handle_host_paste(client, "ls\n");
    assert_eq!(scroll_offset(&runtime, client, pane), 0);
}
