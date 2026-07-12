//! Config reload transaction tests: per-file apply, keybinding
//! all-or-nothing, registry refresh, and pending-sequence clearing.

use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{mpsc, Arc};
use std::time::{Instant, SystemTime};

use koshi_config::conflict::{ConflictDiagnostic, LayerOrigin};
use koshi_config::layer::PartialColorPalette;
use koshi_config::types::{BoundAction, KeybindingsConfig, ModeBindings, ModeName, RgbColor};
use koshi_core::action::{
    ActionHandlerRef, ActionMetadata, ActionNamespace, ActionRef, ActionScope, ActionStatus,
    TargetKind,
};
use koshi_core::geometry::Size;
use koshi_core::ids::{ClientId, PluginId};
use koshi_core::key::{Key, KeyChord, KeySequence, ModFlags};
use koshi_core::lock::LockMode;
use koshi_core::resolve::ActionArgs;
use koshi_observability::cleanup::TerminalCleanupGuard;
use koshi_test_support::fake_pty::FakePtyBackend;

use crate::placeholder::{NullSnapshotProvider, NullStorage};
use crate::runtime::hints::KeyMatch;

fn runtime() -> (Runtime, ClientId) {
    let (tx, rx) = mpsc::channel();
    let mut runtime = Runtime::new(
        Arc::new(FakePtyBackend::new()),
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
    (runtime, client)
}

fn only_session_id(runtime: &Runtime) -> SessionId {
    *runtime.sessions.keys().next().expect("one session")
}

fn sequence(mods: ModFlags, key: char) -> KeySequence {
    KeySequence::new(KeyChord::new(mods, Key::Char(key)), Vec::new())
}

/// A `keybinding.kdl` candidate binding `<C-y>` to `action` in `mode`.
fn candidate_binding(mode: &str, action: ActionRef) -> PartialKeybindingsConfig {
    let mut keys = BTreeMap::new();
    keys.insert(
        sequence(ModFlags::CTRL, 'y'),
        BoundAction {
            action,
            args: ActionArgs::None,
        },
    );
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new(mode),
        ModeBindings {
            keys,
            removed: BTreeSet::new(),
        },
    );
    PartialKeybindingsConfig {
        modes: Some(modes),
        ..PartialKeybindingsConfig::default()
    }
}

fn new_tab_ref() -> ActionRef {
    ActionRef::core("new-tab").expect("valid core action name")
}

#[test]
fn theme_reload_recolors_the_chrome_and_reports_the_session() {
    let (mut runtime, _client) = runtime();
    let session_id = only_session_id(&runtime);

    let events = runtime.reload_theme(PartialThemeConfig {
        name: Some("ocean".to_owned()),
        colors: Some(PartialColorPalette {
            ramp_start: Some(RgbColor::new(0xff, 0x00, 0x00)),
            ..PartialColorPalette::default()
        }),
    });

    assert_eq!(runtime.theme.ramp_start, (0xff, 0x00, 0x00));
    assert_eq!(
        runtime.config.theme.colors.ramp_start,
        RgbColor::new(0xff, 0x00, 0x00)
    );
    assert_eq!(
        events,
        vec![Event::ConfigReloaded(ConfigReloaded { session_id })]
    );
}

#[test]
fn app_config_reload_replaces_the_startup_split_direction() {
    let (mut runtime, _client) = runtime();
    let session_id = only_session_id(&runtime);
    // The constructor seeded `Right` as the app layer.
    assert_eq!(runtime.config.layout.new_pane_direction, Direction::Right);

    let events = runtime.reload_app_config(PartialKoshiConfig {
        layout: Some(PartialLayoutDefaults {
            new_pane_direction: Some(Direction::Down),
            default_layout: None,
        }),
        ..PartialKoshiConfig::default()
    });
    assert_eq!(runtime.config.layout.new_pane_direction, Direction::Down);
    assert_eq!(
        events,
        vec![Event::ConfigReloaded(ConfigReloaded { session_id })]
    );

    // An empty `koshi.kdl` replaces the whole app layer, so the direction
    // falls back to the built-in default rather than the startup seed.
    runtime.reload_app_config(PartialKoshiConfig::default());
    assert_eq!(runtime.config.layout.new_pane_direction, Direction::Right);
}

#[test]
fn valid_keybinding_reload_makes_the_new_binding_resolvable() {
    let (mut runtime, _client) = runtime();
    let session_id = only_session_id(&runtime);
    let key = sequence(ModFlags::CTRL, 'y');
    assert_eq!(
        runtime.keymap_hints.match_sequence(LockMode::Normal, &key),
        KeyMatch::default()
    );

    let outcome = runtime.reload_keybindings(candidate_binding("normal", new_tab_ref()));

    assert_eq!(outcome.report.verdict(), KeymapVerdict::Apply);
    assert_eq!(
        outcome.events,
        vec![Event::ConfigReloaded(ConfigReloaded { session_id })]
    );
    assert_eq!(
        runtime.keymap_hints.match_sequence(LockMode::Normal, &key),
        KeyMatch {
            exact: Some(BoundAction {
                action: new_tab_ref(),
                args: ActionArgs::None,
            }),
            prefix: false,
        }
    );
}

#[test]
fn keybinding_reload_shadowing_the_reserved_unlock_is_kept() {
    let (mut runtime, _client) = runtime();
    let session_id = only_session_id(&runtime);
    let mut keys = BTreeMap::new();
    // `<C-l>` is the reserved unlock; binding it in locked mode is fatal.
    keys.insert(
        KeySequence::new(KeybindingsConfig::RESERVED_UNLOCK, Vec::new()),
        BoundAction {
            action: new_tab_ref(),
            args: ActionArgs::None,
        },
    );
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new("locked"),
        ModeBindings {
            keys,
            removed: BTreeSet::new(),
        },
    );
    let candidate = PartialKeybindingsConfig {
        modes: Some(modes),
        ..PartialKeybindingsConfig::default()
    };

    let before = runtime.config.clone();
    let outcome = runtime.reload_keybindings(candidate);

    assert_eq!(outcome.report.verdict(), KeymapVerdict::Reject);
    assert_eq!(
        outcome.events,
        vec![Event::ConfigReloadFailed(ConfigReloadFailed {
            session_id,
            reason: "the reserved unlock key is bound by user to `core:new-tab` in locked \
                     mode; declare `unlock_alternative` before rebinding it"
                .to_owned(),
        })]
    );
    // Nothing swapped: the running config is byte-for-byte what it was.
    assert_eq!(runtime.config, before);
    let unlock_key = KeySequence::new(KeybindingsConfig::RESERVED_UNLOCK, Vec::new());
    assert_eq!(
        runtime
            .keymap_hints
            .match_sequence(LockMode::Locked, &unlock_key)
            .exact,
        Some(BoundAction {
            action: ActionRef::core("unlock").expect("valid core action name"),
            args: ActionArgs::None,
        })
    );
}

#[test]
fn keybinding_reload_with_zero_chord_depth_is_kept() {
    let (mut runtime, _client) = runtime();
    let session_id = only_session_id(&runtime);
    let candidate = PartialKeybindingsConfig {
        max_chord_depth: Some(0),
        ..PartialKeybindingsConfig::default()
    };

    let before = runtime.config.clone();
    let outcome = runtime.reload_keybindings(candidate);

    // The explicit guard message leads; the unlock-guarantee fatal the empty
    // effective map produces follows in the same joined reason.
    assert_eq!(
        outcome.events,
        vec![Event::ConfigReloadFailed(ConfigReloadFailed {
            session_id,
            reason: "`max_chord_depth` 0 would disable every keybinding including the \
                     locked-mode unlock; the minimum is 1; locked mode has no binding \
                     from `<C-l>` to `core:unlock`; the unlock escape would be \
                     unreachable"
                .to_owned(),
        })]
    );
    assert_eq!(runtime.config, before);
    assert_eq!(runtime.keymap_hints.max_chord_depth(), 4);
}

#[test]
fn keybinding_reload_with_low_depth_drops_overlong_bindings() {
    let (mut runtime, _client) = runtime();
    let session_id = only_session_id(&runtime);
    let long = KeySequence::new(
        KeyChord::new(ModFlags::CTRL, Key::Char('y')),
        vec![KeyChord::new(ModFlags::NONE, Key::Char('x'))],
    );
    let mut keys = BTreeMap::new();
    keys.insert(
        long.clone(),
        BoundAction {
            action: new_tab_ref(),
            args: ActionArgs::None,
        },
    );
    let mut modes = BTreeMap::new();
    modes.insert(
        ModeName::new("normal"),
        ModeBindings {
            keys,
            removed: BTreeSet::new(),
        },
    );
    let candidate = PartialKeybindingsConfig {
        max_chord_depth: Some(1),
        modes: Some(modes),
        ..PartialKeybindingsConfig::default()
    };

    let outcome = runtime.reload_keybindings(candidate);

    // Depth 1 applies — with a warning naming the unreachable binding.
    assert_eq!(outcome.report.verdict(), KeymapVerdict::Apply);
    assert_eq!(
        outcome.report.diagnostics,
        vec![ConflictDiagnostic::ExceedsChordDepth {
            origin: LayerOrigin::User,
            mode: ModeName::new("normal"),
            key: long.clone(),
            action: new_tab_ref(),
            max_chord_depth: 1,
        }]
    );
    assert_eq!(
        outcome.events,
        vec![Event::ConfigReloaded(ConfigReloaded { session_id })]
    );
    // The overlong binding is transparent: no exact match, and its first
    // chord is not a live prefix, so it falls through to the pane.
    assert_eq!(
        runtime.keymap_hints.match_sequence(LockMode::Normal, &long),
        KeyMatch::default()
    );
    assert_eq!(
        runtime
            .keymap_hints
            .match_sequence(LockMode::Normal, &sequence(ModFlags::CTRL, 'y')),
        KeyMatch::default()
    );
    // The shipped two-chord defaults (`<C-p> n` …) fall the same way:
    // `<C-p>` stops being a prefix and falls through.
    assert_eq!(
        runtime
            .keymap_hints
            .match_sequence(LockMode::Normal, &sequence(ModFlags::CTRL, 'p')),
        KeyMatch::default()
    );
    // The one-chord unlock is untouched.
    let unlock_key = KeySequence::new(KeybindingsConfig::RESERVED_UNLOCK, Vec::new());
    assert_eq!(
        runtime
            .keymap_hints
            .match_sequence(LockMode::Locked, &unlock_key)
            .exact,
        Some(BoundAction {
            action: ActionRef::core("unlock").expect("valid core action name"),
            args: ActionArgs::None,
        })
    );
}

#[test]
fn app_config_reload_drops_theme_and_keybinding_sections() {
    let (mut runtime, _client) = runtime();
    let theme_before = runtime.theme;

    runtime.reload_app_config(PartialKoshiConfig {
        theme: Some(PartialThemeConfig {
            name: None,
            colors: Some(PartialColorPalette {
                ramp_start: Some(RgbColor::new(0xff, 0x00, 0x00)),
                ..PartialColorPalette::default()
            }),
        }),
        keybindings: Some(PartialKeybindingsConfig {
            max_chord_depth: Some(0),
            ..PartialKeybindingsConfig::default()
        }),
        ..PartialKoshiConfig::default()
    });

    // Both foreign sections were dropped: the effective config and the
    // resolved theme are exactly what they were.
    assert_eq!(runtime.config, KoshiConfig::default());
    assert_eq!(runtime.theme, theme_before);
    assert_eq!(runtime.keymap_hints.max_chord_depth(), 4);
}

#[test]
fn keybinding_reload_clears_pending_sequences() {
    let (mut runtime, client) = runtime();
    // `<C-p>` opens the default pane prefix, leaving a pending sequence.
    runtime.handle_key_input(
        client,
        KeyChord::new(ModFlags::CTRL, Key::Char('p')),
        vec![0x10],
        Instant::now(),
    );
    let has_pending = |runtime: &Runtime, client: ClientId| {
        runtime
            .session_for_client(client)
            .expect("session")
            .clients
            .get(client)
            .expect("client")
            .pending_key_sequence()
            .is_some()
    };
    assert!(has_pending(&runtime, client));

    runtime.reload_keybindings(candidate_binding("normal", new_tab_ref()));

    assert!(!has_pending(&runtime, client));
}

#[test]
fn registry_refresh_never_swaps_in_a_refused_keymap() {
    let (mut runtime, _client) = runtime();
    let plugin = PluginId::new();
    let action = ActionRef::plugin(plugin, "grab").expect("valid plugin action name");
    let unlock_key = KeySequence::new(KeybindingsConfig::RESERVED_UNLOCK, Vec::new());
    let unlock_bound = BoundAction {
        action: ActionRef::core("unlock").expect("valid core action name"),
        args: ActionArgs::None,
    };

    // A locked-mode binding on the reserved unlock chord to an unregistered
    // plugin action: an orphan, so the reload applies with a warning and the
    // shipped unlock stays live beneath the transparent binding.
    let mut keys = BTreeMap::new();
    keys.insert(
        unlock_key.clone(),
        BoundAction {
            action: action.clone(),
            args: ActionArgs::None,
        },
    );
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
    assert_eq!(outcome.report.verdict(), KeymapVerdict::Apply);
    assert_eq!(
        runtime
            .keymap_hints
            .match_sequence(LockMode::Locked, &unlock_key)
            .exact,
        Some(unlock_bound.clone())
    );

    // The plugin registers the action: the stored binding would now fire and
    // shadow the unlock, so detection refuses — and the refresh must keep
    // the running catalog rather than swap the shadow in.
    runtime
        .action_registry
        .register(
            plugin,
            action.clone(),
            ActionMetadata {
                namespace: ActionNamespace::Plugin(plugin),
                display_name: "Grab".to_owned(),
                description: "Grab the unlock chord".to_owned(),
                scope_class: ActionScope::Client,
                target_compat: vec![TargetKind::Pane],
                args_schema: None,
                handler: ActionHandlerRef::PluginHostCall(plugin),
                status: ActionStatus::Available,
                continuous: false,
            },
        )
        .expect("plugin action registers");
    let report = runtime.refresh_keymap_for_registry();

    assert_eq!(report.verdict(), KeymapVerdict::Reject);
    assert_eq!(
        report.diagnostics,
        vec![ConflictDiagnostic::ReservedUnlockShadowed {
            origin: LayerOrigin::User,
            action,
        }]
    );
    // The running catalog is unchanged: the reserved chord still unlocks.
    assert_eq!(
        runtime
            .keymap_hints
            .match_sequence(LockMode::Locked, &unlock_key)
            .exact,
        Some(unlock_bound)
    );
}

#[test]
fn registry_refresh_turns_an_orphan_binding_live() {
    let (mut runtime, _client) = runtime();
    let plugin = PluginId::new();
    let action = ActionRef::plugin(plugin, "status").expect("valid plugin action name");
    let key = sequence(ModFlags::CTRL, 'y');

    // Bind a plugin action that is not registered yet: the reload applies
    // with an orphan warning and the binding stays transparent.
    let outcome = runtime.reload_keybindings(candidate_binding("normal", action.clone()));
    assert_eq!(outcome.report.verdict(), KeymapVerdict::Apply);
    assert_eq!(
        outcome.report.diagnostics,
        vec![ConflictDiagnostic::OrphanAction {
            origin: LayerOrigin::User,
            mode: ModeName::new("normal"),
            key: key.clone(),
            action: action.clone(),
        }]
    );
    assert_eq!(
        runtime.keymap_hints.match_sequence(LockMode::Normal, &key),
        KeyMatch::default()
    );

    // The plugin registers the action; the refresh re-merges and the same
    // stored binding starts resolving.
    runtime
        .action_registry
        .register(
            plugin,
            action.clone(),
            ActionMetadata {
                namespace: ActionNamespace::Plugin(plugin),
                display_name: "Status".to_owned(),
                description: "Show plugin status".to_owned(),
                scope_class: ActionScope::Client,
                target_compat: vec![TargetKind::Pane],
                args_schema: None,
                handler: ActionHandlerRef::PluginHostCall(plugin),
                status: ActionStatus::Available,
                continuous: false,
            },
        )
        .expect("plugin action registers");
    let report = runtime.refresh_keymap_for_registry();

    assert_eq!(report.diagnostics, Vec::new());
    assert_eq!(
        runtime.keymap_hints.match_sequence(LockMode::Normal, &key),
        KeyMatch {
            exact: Some(BoundAction {
                action,
                args: ActionArgs::None,
            }),
            prefix: false,
        }
    );
}
