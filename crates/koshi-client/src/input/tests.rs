//! Tests for viewer-side key resolution: what a chord means in each mode,
//! how an open sequence captures the keyboard, and the ambiguity deadline.

use super::*;

use std::collections::{BTreeMap, BTreeSet};

use koshi_config::conflict::{KeymapLayer, LayerOrigin};
use koshi_config::hints::KeymapHintCatalog;
use koshi_config::types::{KeybindingsConfig, ModeBindings, ModeName};
use koshi_core::registry::ActionRegistry;

use crate::Client;

/// A viewer on the built-in keymap.
fn build_test_client() -> Client {
    crate::tests::build_test_client_with_event_sender().0
}

fn build_key_chord(modifier_flags: ModFlags, key_character: char) -> KeyChord {
    KeyChord::from_parts(modifier_flags, Key::Char(key_character))
}

/// A resolved keymap holding exactly `bindings` in `mode`, each sequence
/// paired with the core action of that name. Nothing else is bound, so a case
/// the shipped table does not hold can be set up.
fn build_keymap_for_mode(
    mode_name: &str,
    key_bindings: &[(KeySequence, &str)],
) -> KeymapHintCatalog {
    let key_binding_entries = key_bindings
        .iter()
        .map(|(key_sequence, action_name)| {
            (
                key_sequence.clone(),
                BoundAction {
                    action_reference: ActionReference::from_core_action_name(action_name)
                        .expect("valid core action name"),
                    action_arguments: ActionArgs::None,
                },
            )
        })
        .collect();
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text(mode_name),
        ModeBindings {
            bound_action_by_key_sequence: key_binding_entries,
            removed_key_sequences: BTreeSet::new(),
        },
    );
    KeymapHintCatalog::from_parts(
        &[KeymapLayer {
            origin: LayerOrigin::Defaults,
            mode_bindings_by_name,
        }],
        &KeybindingsConfig::default(),
        &ActionRegistry::new(),
    )
}

#[test]
fn an_unbound_key_passes_through_in_normal_mode() {
    let mut client = build_test_client();
    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::NONE, 'a'), Instant::now()),
        KeyOutcome::PassThrough(build_key_chord(ModFlags::NONE, 'a'))
    );
}

#[test]
fn a_prefix_chord_opens_a_sequence_and_types_nothing() {
    // `<C-p>` is the default pane prefix: it binds nothing on its own, so it
    // holds the keyboard rather than reaching the pane.
    let mut client = build_test_client();
    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), Instant::now()),
        KeyOutcome::Pending
    );
    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|pending_key_sequence| pending_key_sequence.list_chords().to_vec()),
        Some(vec![build_key_chord(ModFlags::CTRL, 'p')])
    );
}

#[test]
fn completing_a_sequence_fires_its_binding_and_closes_it() {
    let mut client = build_test_client();
    let current_instant = Instant::now();
    client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), current_instant);

    let key_outcome = client.resolve_key(build_key_chord(ModFlags::NONE, 'n'), current_instant);
    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("`<C-p> n` fires new-pane, got {key_outcome:?}");
    };
    assert_eq!(
        bound_action.action_reference,
        ActionReference::from_core_action_name("new-pane").expect("valid name")
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "a completed sequence closes"
    );
}

#[test]
fn a_key_that_continues_nothing_is_swallowed_and_the_sequence_stands() {
    // The viewer is inside a koshi context: a key that context cannot use goes
    // nowhere rather than surprising the program underneath.
    let mut client = build_test_client();
    let current_instant = Instant::now();
    client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), current_instant);

    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::NONE, 'z'), current_instant,),
        KeyOutcome::Pending,
        "not PassThrough — the pane must not see it"
    );
    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|pending_key_sequence| pending_key_sequence.list_chords().to_vec()),
        Some(vec![build_key_chord(ModFlags::CTRL, 'p')]),
        "the sequence is unchanged"
    );
}

#[test]
fn escape_leaves_an_open_sequence_without_typing_it() {
    let mut client = build_test_client();
    let current_instant = Instant::now();
    client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), current_instant);

    assert_eq!(
        client.resolve_key(ESCAPE_KEY_CHORD, current_instant),
        KeyOutcome::Pending
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "the sequence is gone"
    );
}

#[test]
fn the_unlock_chord_escapes_locked_mode_ahead_of_the_keymap() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);

    let outcome = client.resolve_key(KeybindingsConfig::RESERVED_UNLOCK, Instant::now());
    let KeyOutcome::Fire(bound) = outcome else {
        panic!("the reserved unlock always fires, got {outcome:?}");
    };
    assert_eq!(
        bound.action_reference,
        ActionReference::from_core_action_name("unlock").expect("valid name")
    );
}

#[test]
fn the_unlock_chord_escapes_even_when_the_keymap_lost_its_unlock_binding() {
    // Strip locked mode's bindings out of the resolved keymap entirely — the
    // shape a keymap layer that shadowed or removed the unlock entry would
    // leave. The escape does not read the keymap, so it still fires.
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    let mut mode_bindings_by_name = BTreeMap::new();
    mode_bindings_by_name.insert(
        ModeName::from_text("locked"),
        ModeBindings {
            bound_action_by_key_sequence: BTreeMap::new(),
            removed_key_sequences: BTreeSet::new(),
        },
    );
    client.keymap_catalog = KeymapHintCatalog::from_parts(
        &[KeymapLayer {
            origin: LayerOrigin::Defaults,
            mode_bindings_by_name,
        }],
        &KeybindingsConfig::default(),
        &ActionRegistry::new(),
    );

    let outcome = client.resolve_key(KeybindingsConfig::RESERVED_UNLOCK, Instant::now());

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("the reserved unlock always fires, got {outcome:?}");
    };
    assert_eq!(
        bound.action_reference,
        ActionReference::from_core_action_name("unlock").expect("valid name")
    );
}

#[test]
fn locked_mode_still_passes_keys_it_does_not_bind() {
    // Locked mode is pass-through: that is the whole point of it.
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Locked);
    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::NONE, 'a'), Instant::now()),
        KeyOutcome::PassThrough(build_key_chord(ModFlags::NONE, 'a'))
    );
}

#[test]
fn a_modal_mode_owns_the_keyboard_and_discards_what_it_does_not_bind() {
    let mut client = build_test_client();
    client.set_lock_mode(LockMode::Resize);
    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::NONE, 'a'), Instant::now()),
        KeyOutcome::Discard,
        "a modal layer never leaks a key to the pane"
    );
}

#[test]
fn changing_mode_drops_an_open_sequence() {
    // Held chords were typed at koshi; a mode change is not a request to type
    // them at the pane.
    let mut client = build_test_client();
    client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), Instant::now());
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from(build_key_chord(ModFlags::CTRL, 'p')))
    );

    client.set_lock_mode(LockMode::Locked);
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_prefix_only_sequence_never_wakes_the_loop() {
    // Only exact-plus-longer ambiguity arms a deadline; a prefix that binds
    // nothing on its own waits for its next chord indefinitely.
    let mut client = build_test_client();
    let current_instant = Instant::now();
    client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), current_instant);

    assert_eq!(client.next_key_wakeup(current_instant), None);
    assert_eq!(client.expire_key_sequence(current_instant), None);
}

#[test]
fn expiring_without_a_deadline_fires_nothing() {
    let mut client = build_test_client();
    assert_eq!(client.expire_key_sequence(Instant::now()), None);
}

#[test]
fn a_continuous_binding_re_opens_its_prefix_so_the_last_chord_repeats() {
    // `<C-p> <Left>` fires `core:focus-pane-left`, which the action table marks
    // continuous: the prefix `<C-p>` comes straight back so a second `<Left>`
    // alone focuses left again, with no second `<C-p>`.
    let mut client = build_test_client();
    let current_instant = Instant::now();
    let left_chord = KeyChord::from_parts(ModFlags::NONE, Key::Named(NamedKey::Left));
    let focus_left_action =
        ActionReference::from_core_action_name("focus-pane-left").expect("valid name");

    assert_eq!(
        client.resolve_key(build_key_chord(ModFlags::CTRL, 'p'), current_instant,),
        KeyOutcome::Pending
    );

    let key_outcome = client.resolve_key(left_chord, current_instant);
    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("`<C-p> <Left>` fires focus-pane-left, got {key_outcome:?}");
    };
    assert_eq!(bound_action.action_reference, focus_left_action);
    assert_eq!(
        client
            .get_pending_key_sequence()
            .map(|pending_key_sequence| pending_key_sequence.list_chords().to_vec()),
        Some(vec![build_key_chord(ModFlags::CTRL, 'p')]),
        "the prefix alone is held again, not the whole sequence"
    );
    assert_eq!(
        client.next_key_wakeup(current_instant),
        None,
        "the re-opened prefix waits for its next chord with no deadline"
    );

    let key_outcome = client.resolve_key(left_chord, current_instant);
    let KeyOutcome::Fire(bound_action) = key_outcome else {
        panic!("the bare `<Left>` fires focus-pane-left again, got {key_outcome:?}");
    };
    assert_eq!(bound_action.action_reference, focus_left_action);
}

#[test]
fn a_one_chord_binding_of_a_continuous_action_opens_no_prefix() {
    // Only a multi-chord sequence has a prefix to re-open. `<C-y>` on its own
    // fires `core:focus-pane-left` and leaves the keyboard to the user.
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    client.keymap_catalog =
        build_keymap_for_mode("normal", &[(KeySequence::from(ctrl_y), "focus-pane-left")]);

    let outcome = client.resolve_key(ctrl_y, Instant::now());

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("`<C-y>` fires focus-pane-left, got {outcome:?}");
    };
    assert_eq!(
        bound.action_reference,
        ActionReference::from_core_action_name("focus-pane-left").expect("valid name")
    );
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_sequence_that_is_both_a_binding_and_a_prefix_fires_on_its_deadline() {
    // `<C-y>` binds `core:quit` and also opens `<C-y> a`. The viewer cannot
    // know which the user meant until the deadline passes, and then the
    // complete binding is the answer.
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[
            (KeySequence::from(ctrl_y), "quit"),
            (
                KeySequence::from_first_and_rest(
                    ctrl_y,
                    vec![build_key_chord(ModFlags::NONE, 'a')],
                ),
                "new-tab",
            ),
        ],
    );
    let timeout = client.keymap_catalog.get_chord_timeout();
    let now = Instant::now();

    assert_eq!(client.resolve_key(ctrl_y, now), KeyOutcome::Pending);
    assert_eq!(
        client.next_key_wakeup(now),
        Some(timeout),
        "the ambiguity arms a deadline one chord timeout out"
    );
    assert_eq!(
        client.expire_key_sequence(now),
        None,
        "nothing fires before the deadline"
    );

    let due = now + timeout;
    let bound = client
        .expire_key_sequence(due)
        .expect("the deadline fires the complete binding");

    assert_eq!(
        bound.action_reference,
        ActionReference::from_core_action_name("quit").expect("valid name")
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "the sequence is spent"
    );
    assert_eq!(
        client.next_key_wakeup(due),
        None,
        "and it wakes the loop no more"
    );
}

#[test]
fn the_hint_bar_follows_the_viewers_own_mode() {
    let mut client = build_test_client();
    let normal_mode_hints = client.build_frame_hints(client.get_lock_mode(), false);
    client.set_lock_mode(LockMode::Locked);
    let locked_mode_hints = client.build_frame_hints(client.get_lock_mode(), false);

    assert_ne!(
        normal_mode_hints.hint_bindings, locked_mode_hints.hint_bindings,
        "each mode shows its own bindings"
    );
    assert!(
        locked_mode_hints
            .hint_bindings
            .iter()
            .any(|hint_binding| hint_binding.is_pinned),
        "locked mode pins the unlock hint so truncation cannot drop the escape"
    );
}

#[test]
fn setting_the_mode_the_viewer_is_already_in_leaves_an_open_sequence_alone() {
    // The drop happens on a change of mode, so a report naming the mode the
    // viewer is already in leaves the chords it is holding where they are.
    let mut client = build_test_client();
    let prefix = build_key_chord(ModFlags::CTRL, 'p');
    client.resolve_key(prefix, Instant::now());

    client.set_lock_mode(LockMode::Normal);

    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from(prefix))
    );
}

#[test]
fn the_unlock_chord_takes_a_sequence_open_in_locked_mode_with_it() {
    // A locked-mode keymap can hold a multi-chord binding, so the unlock chord
    // can arrive with chords already held. It is resolved before the sequence
    // buffer, and the held chords go with it.
    let mut client = build_test_client();
    let prefix = build_key_chord(ModFlags::CTRL, 'p');
    client.keymap_catalog = build_keymap_for_mode(
        "locked",
        &[(
            KeySequence::from_first_and_rest(prefix, vec![build_key_chord(ModFlags::NONE, 'n')]),
            "new-pane",
        )],
    );
    client.set_lock_mode(LockMode::Locked);
    let now = Instant::now();
    assert_eq!(client.resolve_key(prefix, now), KeyOutcome::Pending);

    let outcome = client.resolve_key(client.keymap_catalog.get_unlock_chord(), now);

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("the reserved unlock always fires, got {outcome:?}");
    };
    assert_eq!(
        bound.action_reference,
        ActionReference::from_core_action_name("unlock").expect("valid name")
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "the held chord is dropped, never typed at the pane"
    );
}

#[test]
fn a_deadline_already_past_asks_the_loop_to_wake_at_once() {
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[
            (KeySequence::from(ctrl_y), "quit"),
            (
                KeySequence::from_first_and_rest(
                    ctrl_y,
                    vec![build_key_chord(ModFlags::NONE, 'a')],
                ),
                "new-tab",
            ),
        ],
    );
    let timeout = client.keymap_catalog.get_chord_timeout();
    let now = Instant::now();
    assert_eq!(client.resolve_key(ctrl_y, now), KeyOutcome::Pending);

    assert_eq!(
        client.next_key_wakeup(now + timeout + Duration::from_secs(1)),
        Some(Duration::ZERO),
        "a deadline already behind the clock asks for no further wait"
    );
}

#[test]
fn a_keymap_that_retired_the_binding_drops_the_waiting_sequence_instead_of_firing() {
    // The deadline was armed because the sequence was itself a complete
    // binding. A keymap the user reloaded can retire it while it waits, and
    // then the held chords resolve to nothing.
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[
            (KeySequence::from(ctrl_y), "quit"),
            (
                KeySequence::from_first_and_rest(
                    ctrl_y,
                    vec![build_key_chord(ModFlags::NONE, 'a')],
                ),
                "new-tab",
            ),
        ],
    );
    let timeout = client.keymap_catalog.get_chord_timeout();
    let now = Instant::now();
    assert_eq!(client.resolve_key(ctrl_y, now), KeyOutcome::Pending);

    client.keymap_catalog = build_keymap_for_mode("normal", &[]);

    assert_eq!(client.expire_key_sequence(now + timeout), None);
    assert_eq!(
        client.get_pending_key_sequence(),
        None,
        "the held chord is dropped, never typed at the pane"
    );
}

#[test]
fn a_three_chord_binding_fires_only_on_its_third_chord() {
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    let first_following_chord = build_key_chord(ModFlags::NONE, 'a');
    let second_following_chord = build_key_chord(ModFlags::NONE, 'b');
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[(
            KeySequence::from_first_and_rest(
                ctrl_y,
                vec![first_following_chord, second_following_chord],
            ),
            "quit",
        )],
    );
    let now = Instant::now();

    assert_eq!(client.resolve_key(ctrl_y, now), KeyOutcome::Pending);
    assert_eq!(
        client.resolve_key(first_following_chord, now),
        KeyOutcome::Pending
    );
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from_first_and_rest(
            ctrl_y,
            vec![first_following_chord],
        )),
        "both chords are held, in the order they were typed"
    );

    let outcome = client.resolve_key(second_following_chord, now);

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("`<C-y> a b` fires quit, got {outcome:?}");
    };
    assert_eq!(
        bound.action_reference,
        ActionReference::from_core_action_name("quit").expect("valid name")
    );
    assert_eq!(client.get_pending_key_sequence(), None);
}

#[test]
fn a_continuous_three_chord_binding_re_opens_its_two_chord_prefix() {
    // The prefix that comes back is everything but the last chord, so the last
    // chord alone repeats the action.
    let mut client = build_test_client();
    let ctrl_y = build_key_chord(ModFlags::CTRL, 'y');
    let first_following_chord = build_key_chord(ModFlags::NONE, 'a');
    let second_following_chord = build_key_chord(ModFlags::NONE, 'b');
    let focus_left = ActionReference::from_core_action_name("focus-pane-left").expect("valid name");
    client.keymap_catalog = build_keymap_for_mode(
        "normal",
        &[(
            KeySequence::from_first_and_rest(
                ctrl_y,
                vec![first_following_chord, second_following_chord],
            ),
            "focus-pane-left",
        )],
    );
    let now = Instant::now();
    client.resolve_key(ctrl_y, now);
    client.resolve_key(first_following_chord, now);

    let outcome = client.resolve_key(second_following_chord, now);

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("`<C-y> a b` fires focus-pane-left, got {outcome:?}");
    };
    assert_eq!(bound.action_reference, focus_left);
    assert_eq!(
        client.get_pending_key_sequence(),
        Some(&KeySequence::from_first_and_rest(
            ctrl_y,
            vec![first_following_chord],
        )),
        "the two-chord prefix is held again, not the whole sequence"
    );
    assert_eq!(client.next_key_wakeup(now), None);

    let outcome = client.resolve_key(second_following_chord, now);

    let KeyOutcome::Fire(bound) = outcome else {
        panic!("the bare `b` fires focus-pane-left again, got {outcome:?}");
    };
    assert_eq!(bound.action_reference, focus_left);
}
