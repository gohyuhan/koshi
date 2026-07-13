//! End-to-end default-keymap tests: passthrough, lock escape, prefix display,
//! multi-chord dispatch, timeout fallback, mismatch retry, and pane resize.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc};

use koshi_config::conflict::KeymapVerdict;
use koshi_config::layer::PartialKeybindingsConfig;
use koshi_config::types::{BoundAction, ModeBindings, ModeName};
use koshi_core::action::ActionRef;
use koshi_core::geometry::{Direction, Size};
use koshi_core::key::{Key, ModFlags};
use koshi_core::resolve::ActionArgs;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_session::client::Client;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage};
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
        .bootstrap_local(Size { cols: 80, rows: 24 }, SystemTime::UNIX_EPOCH)
        .expect("bootstrap");
    (runtime, fake, client)
}

fn chord(mods: ModFlags, key: char) -> KeyChord {
    KeyChord::new(mods, Key::Char(key))
}

fn only_pane(runtime: &Runtime) -> koshi_core::ids::PaneId {
    *runtime.pty_handles.keys().next().expect("one pane")
}

#[test]
fn unbound_plain_key_passes_to_focused_pty() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    runtime.handle_key_input(
        client,
        chord(ModFlags::NONE, 'a'),
        vec![b'a'],
        Instant::now(),
    );
    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![b'a']]);
}

#[test]
fn the_lock_chord_flips_the_client_both_ways_without_pty_bytes() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    // `<C-l>` locks in normal mode…
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), vec![0x0c], now);
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
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), vec![0x0c], now);
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
    runtime.handle_key_input(
        client,
        chord(ModFlags::CTRL, 'q'),
        vec![0x11],
        Instant::now(),
    );
    assert!(runtime.quit_requested());
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn quit_binding_fires_in_locked_mode_too() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), vec![0x0c], now);
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'q'), vec![0x11], now);
    assert!(runtime.quit_requested());
    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn continuous_resize_keeps_the_prefix_armed_for_repeat_presses() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), vec![b'n'], now);

    // First resize: full `<C-s> h` sequence.
    let sizes_start = runtime.pty_sizes.clone();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 's'), vec![0x13], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'h'), vec![b'h'], now);
    let sizes_once = runtime.pty_sizes.clone();
    assert_ne!(sizes_once, sizes_start);

    // The prefix stayed armed: `h` alone fires the resize again…
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(chord(ModFlags::CTRL, 's')))
    );
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'h'), vec![b'h'], now);
    assert_ne!(runtime.pty_sizes, sizes_once);

    // …and Escape puts the bar back to idle.
    runtime.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        vec![0x1b],
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
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), vec![b'n'], now);
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
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'l'), vec![0x0c], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'x'), vec![b'x'], now);
    assert_eq!(fake.writes(pane).expect("writes"), vec![vec![b'x']]);
}

#[test]
fn pane_prefix_updates_snapshot_then_new_pane_fires() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    assert_eq!(
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        Some(KeySequence::from(chord(ModFlags::CTRL, 'p')))
    );
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), vec![b'n'], now);
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
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
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
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        vec![0x1b],
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
fn unmatched_continuation_flushes_prefix_then_retries_current_key() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'z'), vec![b'z'], now);
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![vec![0x10], vec![b'z']]
    );
}

#[test]
fn directional_focus_binding_moves_focus_across_a_split() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    // Split: the new right pane takes focus.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), vec![b'n'], now);
    let focused_after_split = focused_pane(&runtime, client);

    // `<A-h>` focuses the left neighbor.
    runtime.handle_key_input(client, chord(ModFlags::ALT, 'h'), vec![0x1b, b'h'], now);
    let focused_left = focused_pane(&runtime, client);
    assert_ne!(focused_left, focused_after_split);

    // `<A-l>` returns to the right pane.
    runtime.handle_key_input(client, chord(ModFlags::ALT, 'l'), vec![0x1b, b'l'], now);
    assert_eq!(focused_pane(&runtime, client), focused_after_split);
}

#[test]
fn fullscreen_binding_toggles_the_layout_mode() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), vec![b'n'], now);

    runtime.handle_key_input(client, chord(ModFlags::ALT, 'f'), vec![0x1b, b'f'], now);
    let snap = runtime.build_snapshot(client).expect("snapshot");
    assert_eq!(
        snap.session.active_tab.layout_mode,
        koshi_layout::mode::LayoutMode::Fullscreen {
            focused: focused_pane(&runtime, client)
        }
    );

    runtime.handle_key_input(client, chord(ModFlags::ALT, 'f'), vec![0x1b, b'f'], now);
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
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), vec![b'n'], now);
    let sizes_before = runtime.pty_sizes.clone();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 's'), vec![0x13], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'h'), vec![b'h'], now);
    assert_ne!(runtime.pty_sizes, sizes_before);
}

#[test]
fn continuous_focus_rearm_walks_panes_with_repeated_arrows() {
    let (mut runtime, _fake, client) = runtime();
    let now = Instant::now();
    // Two splits: three panes across, focus on the right-most.
    for _ in 0..2 {
        runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
        runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), vec![b'n'], now);
    }
    let rightmost = focused_pane(&runtime, client);

    // `<C-p> ←` moves one pane left and re-arms the prefix…
    let left = KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Left));
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client, left, vec![0x1b, b'[', b'D'], now);
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
    runtime.handle_key_input(client, left, vec![0x1b, b'[', b'D'], now);
    let leftmost = focused_pane(&runtime, client);
    assert_ne!(leftmost, middle);
    assert_ne!(leftmost, rightmost);
}

#[test]
fn abandoned_rearmed_prefix_writes_nothing_to_the_pane() {
    let (mut runtime, fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), vec![b'n'], now);
    let focused = focused_pane(&runtime, client);

    // Resize once, leave the re-armed prefix hanging, then cancel with Esc:
    // the re-armed prefix carries no fallback bytes, so the shell sees none.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 's'), vec![0x13], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'h'), vec![b'h'], now);
    runtime.handle_key_input(
        client,
        KeyChord::new(ModFlags::NONE, Key::Named(NamedKey::Esc)),
        vec![0x1b],
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
fn rearmed_prefix_mismatch_passes_the_key_through_and_disarms() {
    let (mut runtime, fake, client) = runtime();
    let now = Instant::now();
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), vec![b'n'], now);
    let focused = focused_pane(&runtime, client);

    runtime.handle_key_input(client, chord(ModFlags::CTRL, 's'), vec![0x13], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'h'), vec![b'h'], now);
    let sizes_after_resize = runtime.pty_sizes.clone();

    // `z` matches nothing under `<C-s>`: the empty re-armed prefix flushes
    // nothing, and `z` retries alone — unbound, so it reaches the shell.
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'z'), vec![b'z'], now);
    assert_eq!(fake.writes(focused).expect("writes"), vec![vec![b'z']]);
    assert_eq!(runtime.pty_sizes, sizes_after_resize);
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
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'n'), vec![b'n'], now);
    let focused = focused_pane(&runtime, client);
    let before = runtime.pty_sizes[&focused];

    // The focused pane touches the tab's right edge: `<C-s> l` has no right
    // border to grow through, so its left border moves right — it shrinks.
    runtime.handle_key_input(client, chord(ModFlags::CTRL, 's'), vec![0x13], now);
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'l'), vec![b'l'], now);
    let after = runtime.pty_sizes[&focused];
    assert_eq!(after.cols, before.cols - 1);
    assert_eq!(after.rows, before.rows);
}

/// Bind one `normal`-mode sequence to `action` via [`Runtime::reload_keybindings`].
fn bind_normal(runtime: &mut Runtime, sequence: KeySequence, action: ActionRef, args: ActionArgs) {
    bind_normal_all(runtime, vec![(sequence, action, args)]);
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
fn outer_input_writes_nothing_for_an_unknown_client() {
    let (mut runtime, fake, _client) = runtime();
    let pane = only_pane(&runtime);

    runtime.handle_outer_input(ClientId::new(), b"x");

    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn outer_input_writes_nothing_when_the_client_has_no_focused_pane() {
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

    runtime.handle_outer_input(client, b"x");

    assert_eq!(fake.writes(pane).expect("writes"), Vec::<Vec<u8>>::new());
}

#[test]
fn pending_sequences_stay_independent_across_clients_in_the_same_session() {
    let (mut runtime, fake, client_a) = runtime();
    let now = Instant::now();
    let original_pane = only_pane(&runtime);

    // Split: client A's focus moves to the new pane, leaving `original_pane`
    // unfocused by anyone yet.
    runtime.handle_key_input(client_a, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    runtime.handle_key_input(client_a, chord(ModFlags::NONE, 'n'), vec![b'n'], now);
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
    runtime.handle_key_input(client_a, chord(ModFlags::CTRL, 'p'), vec![0x10], now);
    // ...client B, meanwhile, sends an unrelated unbound key straight through
    // on its own (different) pane.
    runtime.handle_key_input(client_b, chord(ModFlags::NONE, 'z'), vec![b'z'], now);

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
    runtime.handle_key_input(
        client_a,
        chord(ModFlags::CTRL, 'p'),
        vec![0x10],
        Instant::now(),
    );

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
fn a_pending_sequence_beyond_max_chord_depth_flushes_the_whole_buffer_without_firing() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    // A real 4-chord binding at the default max depth: if it fired it would be
    // observable as a new tab, which distinguishes the depth cap's raw-byte
    // flush from the ordinary mismatch-retry path that resolves a held exact
    // binding on a mismatch.
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

    // Park a sequence one chord short of the binding — the exact state a
    // corrupted or stale pending entry would leave behind — then press a 5th
    // chord that pushes the accumulated count past `max_chord_depth` (4).
    runtime
        .session_for_client_mut(client)
        .expect("session")
        .clients
        .get_mut(client)
        .expect("client")
        .update_pending_key_sequence(Some(PendingKeySequence {
            sequence: long,
            raw_bytes: vec![vec![0x19], vec![b'a'], vec![b'b'], vec![b'c']],
            deadline: None,
        }));

    runtime.handle_key_input(
        client,
        chord(ModFlags::NONE, 'z'),
        vec![b'z'],
        Instant::now(),
    );

    // The depth cap dumps every buffered chunk verbatim; the held 4-chord
    // binding never resolves, so no new tab appears.
    assert_eq!(
        fake.writes(pane).expect("writes"),
        vec![vec![0x19], vec![b'a'], vec![b'b'], vec![b'c'], vec![b'z']]
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
        runtime
            .build_snapshot(client)
            .unwrap()
            .client
            .pending_sequence,
        None
    );
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

    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'y'), vec![0x19], now);
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

    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'y'), vec![0x19], now);
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
fn a_held_exact_binding_fires_on_mismatch_then_retries_the_new_key() {
    let (mut runtime, fake, client) = runtime();
    let pane = only_pane(&runtime);
    // `<C-y>` alone is both a complete binding and a prefix of `<C-y> x`.
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

    runtime.handle_key_input(client, chord(ModFlags::CTRL, 'y'), vec![0x19], now);
    // `z` does not extend `<C-y>` into anything: the held `<C-y>` is itself a
    // complete binding, so it fires instead of flushing its raw bytes — firing
    // `new-tab` switches the client's focused pane, so only `z` (unbound on
    // the new tab) retries and reaches the newly focused pane.
    runtime.handle_key_input(client, chord(ModFlags::NONE, 'z'), vec![b'z'], now);

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
    assert_eq!(fake.writes(new_pane).expect("writes"), vec![vec![b'z']]);
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
