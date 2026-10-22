//! Tests for the keymap hint catalog: the defaults-only merge joined to
//! display names, user bindings and removals over the defaults, the firing
//! filter (a refused action, a locked-mode sequence holding the unlock chord,
//! and a mode the build does not register each yield no hint), the
//! chord-depth cap at 0 and 1, the pinned locked-mode unlock, prefix labels
//! under either leader shape, sequence matching, empty modes, the label each
//! frame shows for the move chooser's Enter and for mouse-select, and the
//! per-frame `Arc` sharing.

use koshi_core::key::{BindingModifierFlags, Key, NamedKey};

use super::*;
use crate::keymap_merge::tests::{
    build_character_chord, build_single_chord_sequence, build_two_chord_sequence,
};
use crate::types::tests::{build_bound_action, build_expected_prefix_labels};
use crate::types::ModeBindings;

/// The catalog resolved from a fresh registry — the built-in defaults over
/// the built-in action table, exactly what a stock runtime holds.
fn build_default_hint_catalog() -> KeymapHintCatalog {
    KeymapHintCatalog::from_registry(&ActionRegistry::new())
}

/// The catalog for a user layer holding `bound_action_by_key_sequence` and
/// `removed_key_sequences` in `mode_name`, over
/// the built-in defaults, the default keybinding config and a fresh registry.
fn build_hint_catalog_with_user(
    mode_name: &str,
    bound_action_by_key_sequence: BTreeMap<KeySequence, BoundAction>,
    removed_key_sequences: BTreeSet<KeySequence>,
) -> KeymapHintCatalog {
    let mode_bindings_by_name = BTreeMap::from([(
        ModeName::from_text(mode_name),
        ModeBindings {
            bound_action_by_key_sequence,
            removed_key_sequences,
        },
    )]);
    KeymapHintCatalog::from_keymap_layers_config_and_registry(
        &build_keymap_layers(Some(mode_bindings_by_name), Leader::default()),
        &KeybindingsConfig::default(),
        &ActionRegistry::new(),
    )
}

/// The catalog for the built-in defaults under `keybindings_config`, with the
/// defaults layer built against that config's own leader.
fn build_hint_catalog_with_config(keybindings_config: &KeybindingsConfig) -> KeymapHintCatalog {
    KeymapHintCatalog::from_keymap_layers_config_and_registry(
        &build_keymap_layers(None, keybindings_config.leader),
        keybindings_config,
        &ActionRegistry::new(),
    )
}

#[test]
fn normal_mode_joins_defaults_to_display_names() {
    let hints = build_default_hint_catalog().build_frame_hints(LockMode::Normal, false, false);

    // All 27 shipped normal-mode bindings fire in this build.
    assert_eq!(hints.hint_bindings.len(), 27);

    let new_pane_key_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'p'),
        build_character_chord(BindingModifierFlags::NONE, 'n'),
    );
    let hint_binding = hints
        .hint_bindings
        .iter()
        .find(|hint_binding| hint_binding.key_sequence == new_pane_key_sequence)
        .expect("the default <C-p> n binding yields a hint");
    assert_eq!(hint_binding.action_display_name, "New Pane");
    assert!(!hint_binding.is_user_authored);
    assert!(!hint_binding.is_pinned);
}

#[test]
fn quit_binding_surfaces_in_both_modes() {
    let hint_catalog = build_default_hint_catalog();
    let quit_key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'q');
    for lock_mode in [LockMode::Normal, LockMode::Locked] {
        let hints = hint_catalog.build_frame_hints(lock_mode, false, false);
        let hint_binding = hints
            .hint_bindings
            .iter()
            .find(|hint_binding| hint_binding.key_sequence == quit_key_sequence)
            .unwrap_or_else(|| panic!("{lock_mode:?} binds the quit chord"));
        assert_eq!(hint_binding.action_display_name, "Quit");
    }
}

#[test]
fn locked_mode_pins_the_reserved_unlock() {
    let hints = build_default_hint_catalog().build_frame_hints(LockMode::Locked, false, false);
    // The reserved unlock (the same `<C-l>` that locks in normal mode), the
    // pane placement opener, and the quit and mouse-select chords.
    assert_eq!(hints.hint_bindings.len(), 4);
    let hint_binding = hints
        .hint_bindings
        .iter()
        .find(|hint_binding| {
            hint_binding.key_sequence == KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)
        })
        .expect("locked mode binds the reserved unlock");
    assert_eq!(hint_binding.action_display_name, "Unlock");
    assert!(hint_binding.is_pinned);
}

#[test]
fn modes_without_defaults_are_empty() {
    let hint_catalog = build_default_hint_catalog();
    for lock_mode in [LockMode::Resize, LockMode::TabMode, LockMode::ScrollMode] {
        let hints = hint_catalog.build_frame_hints(lock_mode, false, false);
        assert!(
            hints.hint_bindings.is_empty(),
            "{lock_mode:?} ships no bindings"
        );
        assert!(hints.removed_key_sequences.is_empty());
    }
}

#[test]
fn pane_placement_mode_hints_expose_every_rebindable_placement_action() {
    let hints =
        build_default_hint_catalog().build_frame_hints(LockMode::PanePlacement, false, false);
    let expected_hint_bindings = [
        (
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Left)),
            "Select Pane Target Left",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Down)),
            "Select Pane Target Down",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Up)),
            "Select Pane Target Up",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Right)),
            "Select Pane Target Right",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Named(NamedKey::Left)),
            "Select Pane Insertion Left",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Named(NamedKey::Down)),
            "Select Pane Insertion Down",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Named(NamedKey::Up)),
            "Select Pane Insertion Up",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Named(NamedKey::Right)),
            "Select Pane Insertion Right",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Space)),
            "Cycle Pane Placement Span",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Char('s')),
            "Toggle Pane Placement Stack",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Tab)),
            "Select Next Placement Tab",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::SHIFT, Key::Named(NamedKey::Tab)),
            "Select Previous Placement Tab",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Enter)),
            "Confirm Pane Placement",
        ),
        (
            KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Esc)),
            "Cancel Pane Placement",
        ),
    ];

    assert_eq!(hints.hint_bindings.len(), 14);
    for (key_chord, action_display_name) in expected_hint_bindings {
        let key_sequence = KeySequence::from(key_chord);
        let hint_binding = hints
            .hint_bindings
            .iter()
            .find(|hint_binding| hint_binding.key_sequence == key_sequence)
            .unwrap_or_else(|| panic!("pane placement mode binds {key_sequence}"));
        assert_eq!(hint_binding.action_display_name, action_display_name);
        assert!(!hint_binding.is_user_authored);
        assert!(!hint_binding.is_pinned);
    }
}

#[test]
fn prefix_labels_carry_the_shipped_names() {
    let hints = build_default_hint_catalog().build_frame_hints(LockMode::Normal, false, false);
    assert_eq!(
        *hints.prefix_labels,
        build_expected_prefix_labels(BindingModifierFlags::CTRL)
    );
}

#[test]
fn a_complete_binding_reports_an_exact_match_and_no_longer_sequence() {
    let sequence_match = build_default_hint_catalog().match_sequence(
        LockMode::Normal,
        &build_single_chord_sequence(BindingModifierFlags::CTRL, 'q'),
    );
    assert_eq!(
        sequence_match.exact_bound_action,
        Some(BoundAction {
            action_reference: ActionReference::from_core_action_name("quit")
                .expect("`core:quit` is a valid action name"),
        })
    );
    assert!(
        !sequence_match.has_longer_key_sequence,
        "nothing shipped continues past `<C-q>`, so it is not a prefix of a longer binding"
    );
}

#[test]
fn a_prefix_chord_reports_no_exact_match() {
    let sequence_match = build_default_hint_catalog().match_sequence(
        LockMode::Normal,
        &build_single_chord_sequence(BindingModifierFlags::CTRL, 'p'),
    );
    assert_eq!(sequence_match.exact_bound_action, None);
    assert!(sequence_match.has_longer_key_sequence);
}

#[test]
fn an_unbound_sequence_matches_nothing() {
    let hint_catalog = build_default_hint_catalog();
    assert_eq!(
        hint_catalog.match_sequence(
            LockMode::Normal,
            &build_single_chord_sequence(BindingModifierFlags::CTRL, 'y')
        ),
        KeyMatch::default()
    );
    // A mode nothing binds in holds no map at all.
    assert_eq!(
        hint_catalog.match_sequence(
            LockMode::Resize,
            &build_single_chord_sequence(BindingModifierFlags::CTRL, 'q')
        ),
        KeyMatch::default()
    );
}

#[test]
fn the_configured_unlock_alternative_becomes_the_escape_chord() {
    let alternative_unlock_chord = build_character_chord(BindingModifierFlags::ALT, 'u');
    let keybindings_config = KeybindingsConfig {
        chord_timeout_ms: 1234,
        unlock_alternative: Some(alternative_unlock_chord),
        ..KeybindingsConfig::default()
    };

    let hint_catalog = KeymapHintCatalog::from_keymap_layers_config_and_registry(
        &build_keymap_layers(None, Leader::default()),
        &keybindings_config,
        &ActionRegistry::new(),
    );

    assert_eq!(hint_catalog.get_unlock_chord(), alternative_unlock_chord);
    assert_eq!(
        hint_catalog.get_chord_timeout(),
        Duration::from_millis(1234)
    );
}

#[test]
fn the_unlock_chord_is_the_reserved_one_when_the_config_names_no_alternative() {
    assert_eq!(
        build_default_hint_catalog().get_unlock_chord(),
        KeybindingsConfig::RESERVED_UNLOCK
    );
    assert_eq!(
        build_default_hint_catalog().get_chord_timeout(),
        Duration::from_millis(500)
    );
}

#[test]
fn a_rebound_leader_moves_the_prefix_labels() {
    let keybindings_config = KeybindingsConfig {
        leader: Leader::Modifiers(BindingModifierFlags::ALT),
        ..KeybindingsConfig::default()
    };

    let hints = KeymapHintCatalog::from_keymap_layers_config_and_registry(
        &build_keymap_layers(None, Leader::Modifiers(BindingModifierFlags::ALT)),
        &keybindings_config,
        &ActionRegistry::new(),
    )
    .build_frame_hints(LockMode::Normal, false, false);

    assert_eq!(
        *hints.prefix_labels,
        build_expected_prefix_labels(BindingModifierFlags::ALT)
    );
}

#[test]
fn default_hint_catalog_is_not_marked_reverted_to_defaults() {
    assert!(
        !build_default_hint_catalog()
            .build_frame_hints(LockMode::Normal, false, false)
            .is_reverted_to_defaults
    );
}

#[test]
fn mark_reverted_to_defaults_marks_every_mode_hint() {
    let hint_catalog = build_default_hint_catalog().mark_reverted_to_defaults();
    assert!(
        hint_catalog
            .build_frame_hints(LockMode::Normal, false, false)
            .is_reverted_to_defaults
    );
    assert!(
        hint_catalog
            .build_frame_hints(LockMode::Locked, false, false)
            .is_reverted_to_defaults
    );
}

#[test]
fn frames_share_the_per_mode_data_by_reference() {
    let hint_catalog = build_default_hint_catalog();
    let first_hints = hint_catalog.build_frame_hints(LockMode::Normal, false, false);
    let second_hints = hint_catalog.build_frame_hints(LockMode::Normal, false, false);
    assert!(Arc::ptr_eq(
        &first_hints.hint_bindings,
        &second_hints.hint_bindings
    ));
    assert!(Arc::ptr_eq(
        &first_hints.prefix_labels,
        &second_hints.prefix_labels
    ));
    assert!(Arc::ptr_eq(
        &first_hints.removed_key_sequences,
        &second_hints.removed_key_sequences
    ));
}

#[test]
fn frames_share_each_label_form_by_reference() {
    let hint_catalog = build_default_hint_catalog();
    let label_forms = [(false, false), (false, true), (true, false), (true, true)];
    for (is_mouse_selection_enabled, is_focused_pane_floating) in label_forms {
        let first_hints = hint_catalog.build_frame_hints(
            LockMode::Normal,
            is_mouse_selection_enabled,
            is_focused_pane_floating,
        );
        let second_hints = hint_catalog.build_frame_hints(
            LockMode::Normal,
            is_mouse_selection_enabled,
            is_focused_pane_floating,
        );
        assert!(Arc::ptr_eq(
            &first_hints.hint_bindings,
            &second_hints.hint_bindings
        ));
    }
    // Pane placement mode binds neither `Mouse Select` nor `Move Focused
    // Pane`. All four label forms are one list.
    let pane_placement_hints =
        hint_catalog.build_frame_hints(LockMode::PanePlacement, false, false);
    for (is_mouse_selection_enabled, is_focused_pane_floating) in label_forms {
        assert!(Arc::ptr_eq(
            &pane_placement_hints.hint_bindings,
            &hint_catalog
                .build_frame_hints(
                    LockMode::PanePlacement,
                    is_mouse_selection_enabled,
                    is_focused_pane_floating,
                )
                .hint_bindings
        ));
    }
}

/// How many hint bindings in `keymap_hints` carry `action_display_name`.
fn count_hint_bindings_with_action_display_name(
    keymap_hints: &KeymapHints,
    action_display_name: &str,
) -> usize {
    keymap_hints
        .hint_bindings
        .iter()
        .filter(|hint_binding| hint_binding.action_display_name == action_display_name)
        .count()
}

#[test]
fn frame_hints_label_the_move_choosers_enter_by_the_focused_panes_kind() {
    let hint_catalog = build_default_hint_catalog();
    // Each case: whether the focused pane is floating, then how many entries
    // read `Move Tiled Pane` and `Move Floating Pane`.
    for (is_focused_pane_floating, tiled_hint_count, floating_hint_count) in
        [(false, 2, 1), (true, 1, 2)]
    {
        let move_choice_hints = hint_catalog.build_frame_hints(
            LockMode::PaneMoveChoice,
            false,
            is_focused_pane_floating,
        );

        assert_eq!(
            count_hint_bindings_with_action_display_name(
                &move_choice_hints,
                MOVE_FOCUSED_PANE_HINT
            ),
            0
        );
        assert_eq!(
            count_hint_bindings_with_action_display_name(&move_choice_hints, MOVE_TILED_PANE_HINT),
            tiled_hint_count
        );
        assert_eq!(
            count_hint_bindings_with_action_display_name(
                &move_choice_hints,
                MOVE_FLOATING_PANE_HINT
            ),
            floating_hint_count
        );
    }
}

#[test]
fn frame_hints_flip_the_mouse_select_label_only_while_it_is_on() {
    let hint_catalog = build_default_hint_catalog();
    let mouse_selection_disabled_hints =
        hint_catalog.build_frame_hints(LockMode::Normal, false, false);
    let mouse_selection_enabled_hints =
        hint_catalog.build_frame_hints(LockMode::Normal, true, false);

    assert_eq!(
        mouse_selection_disabled_hints.hint_bindings.len(),
        mouse_selection_enabled_hints.hint_bindings.len()
    );
    let changed_hint_bindings = mouse_selection_disabled_hints
        .hint_bindings
        .iter()
        .zip(mouse_selection_enabled_hints.hint_bindings.iter())
        .filter(|(disabled_hint_binding, enabled_hint_binding)| {
            disabled_hint_binding != enabled_hint_binding
        })
        .collect::<Vec<_>>();
    let [(mouse_select_hint_binding, mouse_unselect_hint_binding)] =
        changed_hint_bindings.as_slice()
    else {
        panic!("exactly one hint changes, got {changed_hint_bindings:?}");
    };
    assert_eq!(
        mouse_select_hint_binding.action_display_name,
        MOUSE_SELECT_HINT
    );
    assert_eq!(
        **mouse_unselect_hint_binding,
        HintBinding {
            action_display_name: MOUSE_UNSELECT_HINT.to_string(),
            ..(*mouse_select_hint_binding).clone()
        }
    );
    assert_eq!(
        mouse_selection_disabled_hints.removed_key_sequences,
        mouse_selection_enabled_hints.removed_key_sequences
    );
    assert_eq!(
        mouse_selection_disabled_hints.is_reverted_to_defaults,
        mouse_selection_enabled_hints.is_reverted_to_defaults
    );
}

#[test]
fn a_user_binding_takes_the_default_key_and_shows_as_user_set() {
    let fullscreen_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'f');
    let hint_catalog = build_hint_catalog_with_user(
        "normal",
        BTreeMap::from([(fullscreen_key_sequence.clone(), build_bound_action("quit"))]),
        BTreeSet::new(),
    );
    let hints = hint_catalog.build_frame_hints(LockMode::Normal, false, false);

    // The user entry replaces the default on that key rather than adding one.
    assert_eq!(hints.hint_bindings.len(), 27);
    let hint_binding = hints
        .hint_bindings
        .iter()
        .find(|hint_binding| hint_binding.key_sequence == fullscreen_key_sequence)
        .expect("the user binding yields a hint");
    assert_eq!(hint_binding.action_display_name, "Quit");
    assert!(hint_binding.is_user_authored);
    assert!(!hint_binding.is_pinned);
    assert_eq!(
        hint_catalog
            .match_sequence(LockMode::Normal, &fullscreen_key_sequence)
            .exact_bound_action,
        Some(build_bound_action("quit"))
    );
}

#[test]
fn a_user_removal_drops_the_hint_and_matches_nothing() {
    let fullscreen_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'f');
    let hint_catalog = build_hint_catalog_with_user(
        "normal",
        BTreeMap::new(),
        BTreeSet::from([fullscreen_key_sequence.clone()]),
    );
    let hints = hint_catalog.build_frame_hints(LockMode::Normal, false, false);

    assert_eq!(hints.hint_bindings.len(), 26);
    assert_eq!(
        hints
            .hint_bindings
            .iter()
            .find(|hint_binding| hint_binding.key_sequence == fullscreen_key_sequence),
        None
    );
    assert_eq!(
        *hints.removed_key_sequences,
        BTreeSet::from([fullscreen_key_sequence.clone()])
    );
    assert_eq!(
        hint_catalog.match_sequence(LockMode::Normal, &fullscreen_key_sequence),
        KeyMatch::default()
    );
    // The removal belongs to the mode that authored it.
    assert_eq!(
        *hint_catalog
            .build_frame_hints(LockMode::Locked, false, false)
            .removed_key_sequences,
        BTreeSet::new()
    );
}

#[test]
fn a_binding_the_resolver_refuses_yields_no_hint() {
    // `core:copy-selection` is not a registered action. The merge drops the
    // binding, and no hint carries it.
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let hint_catalog = build_hint_catalog_with_user(
        "normal",
        BTreeMap::from([(key_sequence.clone(), build_bound_action("copy-selection"))]),
        BTreeSet::new(),
    );
    let hints = hint_catalog.build_frame_hints(LockMode::Normal, false, false);

    assert_eq!(hints.hint_bindings.len(), 27);
    assert_eq!(
        hints
            .hint_bindings
            .iter()
            .find(|hint_binding| hint_binding.key_sequence == key_sequence),
        None
    );
    assert_eq!(
        hint_catalog.match_sequence(LockMode::Normal, &key_sequence),
        KeyMatch::default()
    );
}

#[test]
fn a_sequence_that_both_fires_and_opens_a_longer_one_reports_both() {
    let opening_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'y');
    let longer_sequence = build_two_chord_sequence(
        build_character_chord(BindingModifierFlags::CTRL, 'y'),
        build_character_chord(BindingModifierFlags::NONE, 'a'),
    );
    let hint_catalog = build_hint_catalog_with_user(
        "normal",
        BTreeMap::from([
            (opening_sequence.clone(), build_bound_action("quit")),
            (longer_sequence.clone(), build_bound_action("lock")),
        ]),
        BTreeSet::new(),
    );

    assert_eq!(
        hint_catalog.match_sequence(LockMode::Normal, &opening_sequence),
        KeyMatch {
            exact_bound_action: Some(build_bound_action("quit")),
            has_longer_key_sequence: true,
        }
    );
    assert_eq!(
        hint_catalog.match_sequence(LockMode::Normal, &longer_sequence),
        KeyMatch {
            exact_bound_action: Some(build_bound_action("lock")),
            has_longer_key_sequence: false,
        }
    );
}

#[test]
fn every_locked_entry_firing_unlock_is_pinned() {
    let hint_catalog = build_hint_catalog_with_user(
        "locked",
        BTreeMap::from([(
            build_single_chord_sequence(BindingModifierFlags::ALT, 'u'),
            build_bound_action("unlock"),
        )]),
        BTreeSet::new(),
    );
    let hints = hint_catalog.build_frame_hints(LockMode::Locked, false, false);

    assert_eq!(hints.hint_bindings.len(), 5);
    let pinned_key_sequences: Vec<String> = hints
        .hint_bindings
        .iter()
        .filter(|hint_binding| hint_binding.is_pinned)
        .map(|hint_binding| hint_binding.key_sequence.to_string())
        .collect();
    assert_eq!(pinned_key_sequences, vec!["<C-l>", "<A-u>"]);
}

#[test]
fn a_chord_depth_cap_of_one_drops_every_multi_chord_default() {
    let hint_catalog = build_hint_catalog_with_config(&KeybindingsConfig {
        maximum_chord_depth: 1,
        ..KeybindingsConfig::default()
    });
    let hints = hint_catalog.build_frame_hints(LockMode::Normal, false, false);

    let key_sequence_strings: Vec<String> = hints
        .hint_bindings
        .iter()
        .map(|hint_binding| hint_binding.key_sequence.to_string())
        .collect();
    assert_eq!(
        key_sequence_strings,
        vec!["<Tab>", "<C-g>", "<C-l>", "<C-q>", "<A-f>", "<S-Tab>"]
    );
    // Every pane-group entry under `<C-p>` is two chords long, and at a cap of
    // one `<C-p>` opens nothing.
    assert_eq!(
        hint_catalog.match_sequence(
            LockMode::Normal,
            &build_single_chord_sequence(BindingModifierFlags::CTRL, 'p')
        ),
        KeyMatch::default()
    );
}

#[test]
fn a_chord_leader_collapses_the_groups_and_drops_every_prefix_label() {
    // All three groups open at the leader chord itself. No label names one
    // group, and the catalog offers none.
    let space_leader_chord =
        KeyChord::from_parts(BindingModifierFlags::NONE, Key::Named(NamedKey::Space));
    let hints = build_hint_catalog_with_config(&KeybindingsConfig {
        leader: Leader::Chord(space_leader_chord),
        ..KeybindingsConfig::default()
    })
    .build_frame_hints(LockMode::Normal, false, false);

    assert_eq!(*hints.prefix_labels, BTreeMap::new());
}

#[test]
fn the_chord_timeout_carries_the_configured_milliseconds_at_both_bounds() {
    let minimum_timeout_hint_catalog = build_hint_catalog_with_config(&KeybindingsConfig {
        chord_timeout_ms: 0,
        ..KeybindingsConfig::default()
    });
    assert_eq!(
        minimum_timeout_hint_catalog.get_chord_timeout(),
        Duration::ZERO
    );

    let maximum_timeout_hint_catalog = build_hint_catalog_with_config(&KeybindingsConfig {
        chord_timeout_ms: u32::MAX,
        ..KeybindingsConfig::default()
    });
    assert_eq!(
        maximum_timeout_hint_catalog.get_chord_timeout(),
        Duration::from_millis(4_294_967_295)
    );
}

#[test]
fn mark_reverted_to_defaults_changes_only_the_flag() {
    let base_hint_catalog = build_default_hint_catalog();
    let reverted_hint_catalog = base_hint_catalog.clone().mark_reverted_to_defaults();
    let base_hints = base_hint_catalog.build_frame_hints(LockMode::Normal, false, false);
    let reverted_hints = reverted_hint_catalog.build_frame_hints(LockMode::Normal, false, false);

    assert!(!base_hints.is_reverted_to_defaults);
    assert!(reverted_hints.is_reverted_to_defaults);
    assert!(Arc::ptr_eq(
        &base_hints.hint_bindings,
        &reverted_hints.hint_bindings
    ));
    assert!(Arc::ptr_eq(
        &base_hints.removed_key_sequences,
        &reverted_hints.removed_key_sequences
    ));
    assert!(Arc::ptr_eq(
        &base_hints.prefix_labels,
        &reverted_hints.prefix_labels
    ));
    assert_eq!(
        reverted_hint_catalog.get_unlock_chord(),
        base_hint_catalog.get_unlock_chord()
    );
    assert_eq!(
        reverted_hint_catalog.get_chord_timeout(),
        base_hint_catalog.get_chord_timeout()
    );
    let quit_key_sequence = build_single_chord_sequence(BindingModifierFlags::CTRL, 'q');
    assert_eq!(
        reverted_hint_catalog.match_sequence(LockMode::Normal, &quit_key_sequence),
        base_hint_catalog.match_sequence(LockMode::Normal, &quit_key_sequence)
    );
}

#[test]
fn a_binding_in_a_mode_the_build_does_not_register_yields_no_hint() {
    let key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'u');
    let hint_catalog = build_hint_catalog_with_user(
        "vim",
        BTreeMap::from([(key_sequence.clone(), build_bound_action("quit"))]),
        BTreeSet::new(),
    );

    for lock_mode in LockMode::ALL {
        let hints = hint_catalog.build_frame_hints(lock_mode, false, false);
        assert_eq!(
            hints
                .hint_bindings
                .iter()
                .find(|hint_binding| hint_binding.key_sequence == key_sequence),
            None,
            "{lock_mode:?} carries the binding from the unregistered mode"
        );
        assert_eq!(
            hint_catalog.match_sequence(lock_mode, &key_sequence),
            KeyMatch::default()
        );
    }
    // The shipped defaults are untouched by the skipped mode.
    assert_eq!(
        hint_catalog
            .build_frame_hints(LockMode::Normal, false, false)
            .hint_bindings
            .len(),
        27
    );
}

#[test]
fn a_chord_depth_cap_of_zero_drops_every_binding_and_keeps_the_escape_chord() {
    let hint_catalog = build_hint_catalog_with_config(&KeybindingsConfig {
        maximum_chord_depth: 0,
        ..KeybindingsConfig::default()
    });

    for lock_mode in LockMode::ALL {
        assert_eq!(
            *hint_catalog
                .build_frame_hints(lock_mode, false, false)
                .hint_bindings,
            Vec::<HintBinding>::new(),
            "{lock_mode:?}"
        );
    }
    assert_eq!(
        hint_catalog.match_sequence(
            LockMode::Normal,
            &build_single_chord_sequence(BindingModifierFlags::CTRL, 'q')
        ),
        KeyMatch::default()
    );
    // The unlock chord resolves ahead of the keymap. An empty keymap still
    // reports it.
    assert_eq!(
        hint_catalog.get_unlock_chord(),
        KeybindingsConfig::RESERVED_UNLOCK
    );
}

#[test]
fn a_removal_of_a_key_nothing_binds_is_still_listed_as_removed() {
    let unbound_key_sequence = build_single_chord_sequence(BindingModifierFlags::ALT, 'z');
    let hint_catalog = build_hint_catalog_with_user(
        "normal",
        BTreeMap::new(),
        BTreeSet::from([unbound_key_sequence.clone()]),
    );
    let hints = hint_catalog.build_frame_hints(LockMode::Normal, false, false);

    assert_eq!(hints.hint_bindings.len(), 27);
    assert_eq!(
        *hints.removed_key_sequences,
        BTreeSet::from([unbound_key_sequence.clone()])
    );
    assert_eq!(
        hint_catalog.match_sequence(LockMode::Normal, &unbound_key_sequence),
        KeyMatch::default()
    );
}

#[test]
fn a_locked_sequence_holding_the_unlock_chord_yields_no_hint() {
    let key_sequence = build_two_chord_sequence(
        KeybindingsConfig::RESERVED_UNLOCK,
        build_character_chord(BindingModifierFlags::NONE, 'x'),
    );
    let hint_catalog = build_hint_catalog_with_user(
        "locked",
        BTreeMap::from([(key_sequence.clone(), build_bound_action("quit"))]),
        BTreeSet::new(),
    );
    let hints = hint_catalog.build_frame_hints(LockMode::Locked, false, false);

    assert_eq!(hints.hint_bindings.len(), 4);
    assert_eq!(
        hints
            .hint_bindings
            .iter()
            .find(|hint_binding| hint_binding.key_sequence == key_sequence),
        None
    );
    assert_eq!(
        hint_catalog.match_sequence(LockMode::Locked, &key_sequence),
        KeyMatch::default()
    );
    // The one-chord unlock stays live, and the dropped sequence opens nothing.
    assert_eq!(
        hint_catalog.match_sequence(
            LockMode::Locked,
            &KeySequence::from(KeybindingsConfig::RESERVED_UNLOCK)
        ),
        KeyMatch {
            exact_bound_action: Some(build_bound_action("unlock")),
            has_longer_key_sequence: false,
        }
    );
}

#[test]
fn entries_are_sorted_by_key_sequence_with_no_key_twice() {
    let hints = build_default_hint_catalog().build_frame_hints(LockMode::Normal, false, false);
    let key_sequences: Vec<KeySequence> = hints
        .hint_bindings
        .iter()
        .map(|hint_binding| hint_binding.key_sequence.clone())
        .collect();

    let mut sorted_unique_key_sequences = key_sequences.clone();
    sorted_unique_key_sequences.sort();
    sorted_unique_key_sequences.dedup();
    assert_eq!(key_sequences, sorted_unique_key_sequences);
}
