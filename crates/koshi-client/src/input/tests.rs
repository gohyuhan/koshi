//! Tests for viewer-side key resolution: what a chord means in each mode,
//! how an open sequence captures the keyboard, and the ambiguity deadline.

use super::*;

use std::sync::mpsc;

use koshi_config::conflict::keymap_layers;
use koshi_config::key::Leader;
use koshi_config::types::KeybindingsConfig;
use koshi_core::registry::ActionRegistry;
use koshi_observability::cleanup::TerminalCleanupGuard;

use crate::Client;

/// A viewer on the built-in keymap.
fn client() -> Client {
    let (_tx, rx) = mpsc::sync_channel(8);
    let registry = ActionRegistry::new();
    let keybindings = KeybindingsConfig::default();
    let keymap = koshi_config::hints::KeymapHintCatalog::from_parts(
        &keymap_layers(None, Leader::default()),
        &keybindings,
        &registry,
    );
    Client::new(
        koshi_core::ids::ClientId::new(),
        koshi_core::geometry::Size { cols: 80, rows: 24 },
        rx,
        koshi_config::types::ClientConfig::default(),
        keymap,
        TerminalCleanupGuard::new(),
    )
}

fn chord(mods: ModFlags, key: char) -> KeyChord {
    KeyChord::new(mods, Key::Char(key))
}

#[test]
fn an_unbound_key_passes_through_in_normal_mode() {
    let mut client = client();
    assert_eq!(
        client.resolve_key(chord(ModFlags::NONE, 'a'), Instant::now()),
        KeyOutcome::PassThrough(chord(ModFlags::NONE, 'a'))
    );
}

#[test]
fn a_prefix_chord_opens_a_sequence_and_types_nothing() {
    // `<C-p>` is the default pane prefix: it binds nothing on its own, so it
    // holds the keyboard rather than reaching the pane.
    let mut client = client();
    assert_eq!(
        client.resolve_key(chord(ModFlags::CTRL, 'p'), Instant::now()),
        KeyOutcome::Pending
    );
    assert_eq!(
        client.pending_sequence().map(|s| s.chords().to_vec()),
        Some(vec![chord(ModFlags::CTRL, 'p')])
    );
}

#[test]
fn completing_a_sequence_fires_its_binding_and_closes_it() {
    let mut client = client();
    let now = Instant::now();
    client.resolve_key(chord(ModFlags::CTRL, 'p'), now);

    let outcome = client.resolve_key(chord(ModFlags::NONE, 'n'), now);
    let KeyOutcome::Fire(bound) = outcome else {
        panic!("`<C-p> n` fires new-pane, got {outcome:?}");
    };
    assert_eq!(
        bound.action,
        ActionRef::core("new-pane").expect("valid name")
    );
    assert!(
        client.pending_sequence().is_none(),
        "a completed sequence closes"
    );
}

#[test]
fn a_key_that_continues_nothing_is_swallowed_and_the_sequence_stands() {
    // The viewer is inside a koshi context: a key that context cannot use goes
    // nowhere rather than surprising the program underneath.
    let mut client = client();
    let now = Instant::now();
    client.resolve_key(chord(ModFlags::CTRL, 'p'), now);

    assert_eq!(
        client.resolve_key(chord(ModFlags::NONE, 'z'), now),
        KeyOutcome::Pending,
        "not PassThrough — the pane must not see it"
    );
    assert_eq!(
        client.pending_sequence().map(|s| s.chords().to_vec()),
        Some(vec![chord(ModFlags::CTRL, 'p')]),
        "the sequence is unchanged"
    );
}

#[test]
fn escape_leaves_an_open_sequence_without_typing_it() {
    let mut client = client();
    let now = Instant::now();
    client.resolve_key(chord(ModFlags::CTRL, 'p'), now);

    assert_eq!(client.resolve_key(ESCAPE, now), KeyOutcome::Pending);
    assert!(client.pending_sequence().is_none(), "the sequence is gone");
}

#[test]
fn the_unlock_chord_escapes_locked_mode_ahead_of_the_keymap() {
    let mut client = client();
    client.set_lock_mode(LockMode::Locked);

    let outcome = client.resolve_key(KeybindingsConfig::RESERVED_UNLOCK, Instant::now());
    let KeyOutcome::Fire(bound) = outcome else {
        panic!("the reserved unlock always fires, got {outcome:?}");
    };
    assert_eq!(bound.action, ActionRef::core("unlock").expect("valid name"));
}

#[test]
fn locked_mode_still_passes_keys_it_does_not_bind() {
    // Locked mode is pass-through: that is the whole point of it.
    let mut client = client();
    client.set_lock_mode(LockMode::Locked);
    assert_eq!(
        client.resolve_key(chord(ModFlags::NONE, 'a'), Instant::now()),
        KeyOutcome::PassThrough(chord(ModFlags::NONE, 'a'))
    );
}

#[test]
fn a_modal_mode_owns_the_keyboard_and_discards_what_it_does_not_bind() {
    let mut client = client();
    client.set_lock_mode(LockMode::Resize);
    assert_eq!(
        client.resolve_key(chord(ModFlags::NONE, 'a'), Instant::now()),
        KeyOutcome::Discard,
        "a modal layer never leaks a key to the pane"
    );
}

#[test]
fn changing_mode_drops_an_open_sequence() {
    // Held chords were typed at koshi; a mode change is not a request to type
    // them at the pane.
    let mut client = client();
    client.resolve_key(chord(ModFlags::CTRL, 'p'), Instant::now());
    assert!(client.pending_sequence().is_some());

    client.set_lock_mode(LockMode::Locked);
    assert!(client.pending_sequence().is_none());
}

#[test]
fn a_prefix_only_sequence_never_wakes_the_loop() {
    // Only exact-plus-longer ambiguity arms a deadline; a prefix that binds
    // nothing on its own waits for its next chord indefinitely.
    let mut client = client();
    let now = Instant::now();
    client.resolve_key(chord(ModFlags::CTRL, 'p'), now);

    assert_eq!(client.next_key_wakeup(now), None);
    assert_eq!(client.expire_key_sequence(now), None);
}

#[test]
fn expiring_without_a_deadline_fires_nothing() {
    let mut client = client();
    assert_eq!(client.expire_key_sequence(Instant::now()), None);
}

#[test]
fn a_reloaded_keymap_drops_the_sequence_being_typed() {
    let mut client = client();
    let registry = ActionRegistry::new();
    client.resolve_key(chord(ModFlags::CTRL, 'p'), Instant::now());
    assert!(client.pending_sequence().is_some());

    client.set_keymap(koshi_config::hints::KeymapHintCatalog::from_parts(
        &keymap_layers(None, Leader::default()),
        &KeybindingsConfig::default(),
        &registry,
    ));

    assert!(
        client.pending_sequence().is_none(),
        "held chords reached for bindings the new keymap may not have"
    );
}

#[test]
fn the_hint_bar_follows_the_viewers_own_mode() {
    let mut client = client();
    let normal = client.keymap_hints();
    client.set_lock_mode(LockMode::Locked);
    let locked = client.keymap_hints();

    assert_ne!(
        normal.entries, locked.entries,
        "each mode shows its own bindings"
    );
    assert!(
        locked.entries.iter().any(|entry| entry.pinned),
        "locked mode pins the unlock hint so truncation cannot drop the escape"
    );
}
