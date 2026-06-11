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
fn action_ref_rejects_invalid_name_on_decode() {
    let forged = r#"{"namespace":"Core","name":"Bad Name"}"#;
    let decoded: Result<ActionRef, _> = serde_json::from_str(forged);
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
        source_doc_ref: Some(Cow::Borrowed("TILE_04")),
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
        assert!(metadata.source_doc_ref.is_some());
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
