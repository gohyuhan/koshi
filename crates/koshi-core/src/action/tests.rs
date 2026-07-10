//! Tests for the action vocabulary.

use super::*;
use crate::ids::PluginId;
use std::collections::BTreeSet;

/// Roundtrip a value through JSON and assert it survives unchanged.
fn roundtrip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(*value, back);
}

#[test]
fn action_name_accepts_valid_grammar() {
    for name in ["a", "new-pane", "copy-mode-search-prev", "x9", "a-1-b-2"] {
        assert!(ActionName::new(name).is_ok(), "{name:?} should be valid");
    }
    // Exactly the maximum length (1 + 30) is allowed.
    let max = format!("a{}", "b".repeat(MAX_ACTION_NAME_LEN - 1));
    assert_eq!(max.len(), MAX_ACTION_NAME_LEN);
    assert!(ActionName::new(&max).is_ok());
}

#[test]
fn action_name_rejects_invalid_grammar() {
    assert_eq!(ActionName::new(""), Err(ActionNameError::Empty));
    assert_eq!(
        ActionName::new("New"),
        Err(ActionNameError::InvalidStart { ch: 'N' })
    );
    assert_eq!(
        ActionName::new("1pane"),
        Err(ActionNameError::InvalidStart { ch: '1' })
    );
    assert_eq!(
        ActionName::new("-pane"),
        Err(ActionNameError::InvalidStart { ch: '-' })
    );
    assert_eq!(
        ActionName::new("new_pane"),
        Err(ActionNameError::InvalidChar { ch: '_' })
    );
    assert_eq!(
        ActionName::new("newPane"),
        Err(ActionNameError::InvalidChar { ch: 'P' })
    );
    let too_long = format!("a{}", "b".repeat(MAX_ACTION_NAME_LEN));
    assert_eq!(
        ActionName::new(&too_long),
        Err(ActionNameError::TooLong {
            len: MAX_ACTION_NAME_LEN + 1
        })
    );
}

#[test]
fn action_name_serde_validates_on_decode() {
    roundtrip(&ActionName::new("focus-pane").expect("valid"));
    let decoded: Result<ActionName, _> = serde_json::from_str("\"BadName\"");
    assert!(decoded.is_err(), "invalid name must not deserialize");
}

#[test]
fn action_ref_display_per_namespace() {
    let core = ActionRef::core("new-pane").expect("valid");
    assert_eq!(core.to_string(), "core:new-pane");

    let user = ActionRef::user("my-macro").expect("valid");
    assert_eq!(user.to_string(), "user:my-macro");

    let plugin_id = PluginId::new();
    let plugin = ActionRef::plugin(plugin_id, "open-status").expect("valid");
    assert_eq!(
        plugin.to_string(),
        format!("plugin:{}:open-status", plugin_id.as_uuid())
    );
}

#[test]
fn action_ref_roundtrips_each_namespace() {
    roundtrip(&ActionRef::core("close-pane").expect("valid"));
    roundtrip(&ActionRef::user("workflow-1").expect("valid"));
    roundtrip(&ActionRef::plugin(PluginId::new(), "diff").expect("valid"));
}

#[test]
fn action_ref_serializes_as_canonical_string() {
    // The wire form is the documented `core:new-pane` token, not a struct, so a
    // keymap referencing actions by name decodes straight into an `ActionRef`.
    let core = ActionRef::core("new-pane").expect("valid");
    assert_eq!(
        serde_json::to_string(&core).expect("serialize"),
        "\"core:new-pane\""
    );

    let decoded: ActionRef = serde_json::from_str("\"core:new-pane\"").expect("deserialize");
    assert_eq!(decoded, core);
}

#[test]
fn action_ref_parses_canonical_string() {
    assert_eq!(
        "core:new-pane".parse::<ActionRef>().expect("valid"),
        ActionRef::core("new-pane").expect("valid")
    );
    assert_eq!(
        "user:my-macro".parse::<ActionRef>().expect("valid"),
        ActionRef::user("my-macro").expect("valid")
    );

    let plugin_id = PluginId::new();
    let text = format!("plugin:{}:open-status", plugin_id.as_uuid());
    assert_eq!(
        text.parse::<ActionRef>().expect("valid"),
        ActionRef::plugin(plugin_id, "open-status").expect("valid")
    );
}

#[test]
fn action_ref_rejects_malformed_strings() {
    assert_eq!(
        "new-pane".parse::<ActionRef>(),
        Err(ActionRefParseError::MissingNamespace)
    );
    assert!(matches!(
        "shell:new-pane".parse::<ActionRef>(),
        Err(ActionRefParseError::UnknownNamespace { .. })
    ));
    assert_eq!(
        "plugin:not-a-uuid:x".parse::<ActionRef>(),
        Err(ActionRefParseError::InvalidPluginId)
    );
    assert_eq!(
        format!("plugin:{}", PluginId::new().as_uuid()).parse::<ActionRef>(),
        Err(ActionRefParseError::MissingPluginName)
    );
    assert!(matches!(
        "core:Bad Name".parse::<ActionRef>(),
        Err(ActionRefParseError::Name(_))
    ));

    // The same rejection holds when decoding from the wire.
    let decoded: Result<ActionRef, _> = serde_json::from_str("\"core:Bad Name\"");
    assert!(decoded.is_err(), "invalid action name must not deserialize");
}

#[test]
fn handler_ref_roundtrips() {
    roundtrip(&ActionHandlerRef::CoreCommand(CommandKind::NewPane));
    roundtrip(&ActionHandlerRef::PluginHostCall(PluginId::new()));
    roundtrip(&ActionHandlerRef::Sequence(vec![
        ActionRef::core("lock").expect("valid"),
        ActionRef::core("new-tab").expect("valid"),
    ]));
}

#[test]
fn action_metadata_roundtrips() {
    let metadata = ActionMetadata {
        namespace: ActionNamespace::Core,
        display_name: "New Pane".to_string(),
        description: "Split the focused pane".to_string(),
        scope_class: ActionScope::PaneSession,
        target_compat: vec![TargetKind::Pane],
        args_schema: Some(ActionArgsSchema::default()),
        handler: ActionHandlerRef::CoreCommand(CommandKind::NewPane),
        status: ActionStatus::Available,
    };
    roundtrip(&metadata);
}

#[test]
fn core_seeds_are_well_formed() {
    let seeds = core_action_seeds();

    // Every seed is in the core namespace, on both the ref and its metadata.
    for (action, metadata) in &seeds {
        assert_eq!(action.namespace, ActionNamespace::Core);
        assert_eq!(metadata.namespace, ActionNamespace::Core);
    }

    // No duplicate action refs.
    let unique: BTreeSet<String> = seeds.iter().map(|(a, _)| a.to_string()).collect();
    assert_eq!(
        unique.len(),
        seeds.len(),
        "seed action names must be unique"
    );

    // The whole table roundtrips through serde.
    for (action, metadata) in &seeds {
        roundtrip(action);
        roundtrip(metadata);
    }
}

/// Pins the client-scoped seeds: lock mode and focus are per-client state, so
/// their actions carry the `Client` scope and accept a client target.
#[test]
fn lock_and_focus_seeds_are_client_scoped() {
    let seeds = core_action_seeds();
    let metadata_of = |name: &str| {
        let action = ActionRef::core(name).expect("valid seed name");
        seeds
            .iter()
            .find(|(seeded, _)| *seeded == action)
            .unwrap_or_else(|| panic!("{name} must be seeded"))
            .1
            .clone()
    };

    let cases: &[(&str, Vec<TargetKind>)] = &[
        ("focus-pane", vec![TargetKind::Pane, TargetKind::Client]),
        ("focus-tab", vec![TargetKind::Tab, TargetKind::Client]),
        ("next-tab", vec![TargetKind::Client]),
        ("previous-tab", vec![TargetKind::Client]),
        ("lock", vec![TargetKind::Client]),
        ("unlock", vec![TargetKind::Client]),
        ("toggle-lock", vec![TargetKind::Client]),
    ];
    for (name, targets) in cases {
        let metadata = metadata_of(name);
        assert_eq!(metadata.scope_class, ActionScope::Client, "for {name}");
        assert_eq!(metadata.target_compat, *targets, "for {name}");
    }
}

/// Pins which seeds are coming-soon: the copy-mode and plugin command families
/// and `quit` have no runtime handler yet, so each is seeded `ComingSoon` and
/// every other action is `Available`. When one lands, its `core_seed`
/// declaration flips and this list shrinks.
#[test]
fn coming_soon_seeds_are_pinned() {
    let mut coming_soon: Vec<String> = core_action_seeds()
        .iter()
        .filter(|(_, metadata)| metadata.status == ActionStatus::ComingSoon)
        .map(|(action, _)| action.to_string())
        .collect();
    coming_soon.sort();

    let mut expected = [
        "core:copy-mode-clear-selection",
        "core:copy-mode-copy",
        "core:copy-mode-enter",
        "core:copy-mode-exit",
        "core:copy-mode-move-cursor",
        "core:copy-mode-search",
        "core:copy-mode-search-next",
        "core:copy-mode-search-prev",
        "core:copy-mode-set-selection",
        "core:plugin-disable",
        "core:plugin-enable",
        "core:plugin-install",
        "core:plugin-reload",
        "core:plugin-uninstall",
        "core:plugin-update",
        "core:quit",
    ]
    .map(String::from)
    .to_vec();
    expected.sort();

    assert_eq!(coming_soon, expected);
}

/// Pins the exact set of built-in actions. Adding, removing, or renaming a seed
/// changes this list and fails the assert — a deliberate gate so the stable
/// user-facing surface never shifts silently.
#[test]
fn core_seed_snapshot_is_stable() {
    let mut names: Vec<String> = core_action_seeds()
        .iter()
        .map(|(action, _)| action.to_string())
        .collect();
    names.sort();

    let expected = vec![
        "core:close-pane",
        "core:close-tab",
        "core:copy-mode-clear-selection",
        "core:copy-mode-copy",
        "core:copy-mode-enter",
        "core:copy-mode-exit",
        "core:copy-mode-move-cursor",
        "core:copy-mode-search",
        "core:copy-mode-search-next",
        "core:copy-mode-search-prev",
        "core:copy-mode-set-selection",
        "core:focus-pane",
        "core:focus-tab",
        "core:lock",
        "core:move-tab",
        "core:new-pane",
        "core:new-tab",
        "core:next-tab",
        "core:plugin-disable",
        "core:plugin-enable",
        "core:plugin-install",
        "core:plugin-reload",
        "core:plugin-uninstall",
        "core:plugin-update",
        "core:previous-tab",
        "core:quit",
        "core:rename-pane",
        "core:rename-session",
        "core:rename-tab",
        "core:resize-pane",
        "core:run",
        "core:toggle-lock",
        "core:toggle-pane-fullscreen",
        "core:unlock",
    ];
    assert_eq!(names, expected);
}
