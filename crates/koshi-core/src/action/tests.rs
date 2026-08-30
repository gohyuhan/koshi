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
    for name in [
        "a",
        "new-pane",
        "toggle-pane-fullscreen",
        "x9",
        "a-1-b-2",
        "a-",
        "a--b",
    ] {
        assert_eq!(
            ActionName::new(name).map(String::from),
            Ok(name.to_string()),
            "{name:?} should be valid"
        );
    }
    // Exactly the maximum length (1 + 30) is allowed.
    let max = format!("a{}", "b".repeat(MAX_ACTION_NAME_LEN - 1));
    assert_eq!(max.len(), MAX_ACTION_NAME_LEN);
    assert_eq!(ActionName::new(&max).map(String::from), Ok(max.clone()));
}

#[test]
fn action_name_rejects_non_ascii_characters() {
    assert_eq!(
        ActionName::new("é"),
        Err(ActionNameError::InvalidStart { ch: 'é' })
    );
    assert_eq!(
        ActionName::new("aé"),
        Err(ActionNameError::InvalidChar { ch: 'é' })
    );
    assert_eq!(
        ActionName::new("a😀"),
        Err(ActionNameError::InvalidChar { ch: '😀' })
    );
    assert_eq!(
        ActionName::new("a b"),
        Err(ActionNameError::InvalidChar { ch: ' ' })
    );
}

#[test]
fn action_name_error_messages_are_pinned() {
    assert_eq!(ActionNameError::Empty.to_string(), "action name is empty");
    assert_eq!(
        ActionNameError::TooLong { len: 32 }.to_string(),
        "action name is 32 chars; the maximum is 31"
    );
    assert_eq!(
        ActionNameError::InvalidStart { ch: 'N' }.to_string(),
        "action name must start with a lowercase letter, found 'N'"
    );
    assert_eq!(
        ActionNameError::InvalidChar { ch: '_' }.to_string(),
        "action name may only contain [a-z0-9-], found '_'"
    );
}

#[test]
fn action_name_reads_back_as_the_input_string() {
    let name = ActionName::new("focus-pane").expect("valid");
    assert_eq!(name.as_str(), "focus-pane");
    assert_eq!(name.to_string(), "focus-pane");
    assert_eq!(String::from(name.clone()), "focus-pane");
    assert_eq!(
        serde_json::to_string(&name).expect("serialize"),
        "\"focus-pane\""
    );
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
fn invalid_char_is_reported_even_when_the_name_is_also_too_long() {
    // The char-grammar scan runs over every character before the length
    // check, so a bad character anywhere — even past the length cap — wins
    // over `TooLong`, per the documented precedence.
    let name = format!("a{}_", "b".repeat(40));
    assert!(name.chars().count() > MAX_ACTION_NAME_LEN);
    assert_eq!(
        ActionName::new(&name),
        Err(ActionNameError::InvalidChar { ch: '_' })
    );
}

#[test]
fn action_name_serde_validates_on_decode() {
    roundtrip(&ActionName::new("focus-pane").expect("valid"));
    let decoded: Result<ActionName, _> = serde_json::from_str("\"BadName\"");
    assert_eq!(
        decoded
            .expect_err("invalid name must not deserialize")
            .to_string(),
        "action name must start with a lowercase letter, found 'B'"
    );
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
    assert_eq!(
        "shell:new-pane".parse::<ActionRef>(),
        Err(ActionRefParseError::UnknownNamespace {
            found: "shell".to_string()
        })
    );
    assert_eq!(
        "plugin:not-a-uuid:x".parse::<ActionRef>(),
        Err(ActionRefParseError::InvalidPluginId)
    );
    assert_eq!(
        format!("plugin:{}", PluginId::new().as_uuid()).parse::<ActionRef>(),
        Err(ActionRefParseError::MissingPluginName)
    );
    assert_eq!(
        "core:Bad Name".parse::<ActionRef>(),
        Err(ActionRefParseError::Name(ActionNameError::InvalidStart {
            ch: 'B'
        }))
    );

    // The same rejection holds when decoding from the wire.
    let decoded: Result<ActionRef, _> = serde_json::from_str("\"core:Bad Name\"");
    assert_eq!(
        decoded
            .expect_err("invalid action name must not deserialize")
            .to_string(),
        "action name must start with a lowercase letter, found 'B'"
    );
}

#[test]
fn action_ref_parse_reports_the_first_failing_rule() {
    let plugin_id = PluginId::new();
    let uuid = plugin_id.as_uuid();
    let cases: &[(String, ActionRefParseError)] = &[
        (String::new(), ActionRefParseError::MissingNamespace),
        ("core".to_string(), ActionRefParseError::MissingNamespace),
        (
            ":".to_string(),
            ActionRefParseError::UnknownNamespace {
                found: String::new(),
            },
        ),
        (
            "CORE:new-pane".to_string(),
            ActionRefParseError::UnknownNamespace {
                found: "CORE".to_string(),
            },
        ),
        (
            " core:new-pane".to_string(),
            ActionRefParseError::UnknownNamespace {
                found: " core".to_string(),
            },
        ),
        (
            "core:".to_string(),
            ActionRefParseError::Name(ActionNameError::Empty),
        ),
        (
            "user:".to_string(),
            ActionRefParseError::Name(ActionNameError::Empty),
        ),
        (
            "core:new-pane:x".to_string(),
            ActionRefParseError::Name(ActionNameError::InvalidChar { ch: ':' }),
        ),
        (
            "plugin:".to_string(),
            ActionRefParseError::MissingPluginName,
        ),
        (
            "plugin:not-a-uuid".to_string(),
            ActionRefParseError::MissingPluginName,
        ),
        (
            "plugin::x".to_string(),
            ActionRefParseError::InvalidPluginId,
        ),
        (
            format!("plugin:{uuid}:"),
            ActionRefParseError::Name(ActionNameError::Empty),
        ),
        (
            format!("plugin:{uuid}:a:b"),
            ActionRefParseError::Name(ActionNameError::InvalidChar { ch: ':' }),
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(
            text.parse::<ActionRef>(),
            Err(expected.clone()),
            "for {text:?}"
        );
    }
}

#[test]
fn action_ref_parse_error_messages_are_pinned() {
    assert_eq!(
        ActionRefParseError::MissingNamespace.to_string(),
        "action ref is missing a 'namespace:' prefix"
    );
    assert_eq!(
        ActionRefParseError::UnknownNamespace {
            found: "shell".to_string()
        }
        .to_string(),
        "unknown action namespace \"shell\"; expected core, plugin, or user"
    );
    assert_eq!(
        ActionRefParseError::MissingPluginName.to_string(),
        "plugin action ref must be 'plugin:<uuid>:<name>'"
    );
    assert_eq!(
        ActionRefParseError::InvalidPluginId.to_string(),
        "plugin action ref has an invalid UUID"
    );
    assert_eq!(
        ActionRefParseError::Name(ActionNameError::Empty).to_string(),
        "action name is empty"
    );
}

#[test]
fn action_ref_parse_error_source_is_the_name_error_only() {
    use std::error::Error;

    let name_error = ActionRefParseError::Name(ActionNameError::Empty);
    assert_eq!(
        name_error.source().map(ToString::to_string),
        Some("action name is empty".to_string())
    );
    for error in [
        ActionRefParseError::MissingNamespace,
        ActionRefParseError::UnknownNamespace {
            found: "shell".to_string(),
        },
        ActionRefParseError::MissingPluginName,
        ActionRefParseError::InvalidPluginId,
    ] {
        assert_eq!(
            error.source().map(ToString::to_string),
            None,
            "for {error:?}"
        );
    }
}

#[test]
fn action_ref_parse_accepts_a_plugin_uuid_without_hyphens() {
    let plugin_id = PluginId::new();
    let text = format!("plugin:{}:x", plugin_id.as_uuid().simple());
    let parsed = text.parse::<ActionRef>().expect("valid");
    assert_eq!(parsed, ActionRef::plugin(plugin_id, "x").expect("valid"));
    // The canonical form always prints the hyphenated UUID.
    assert_eq!(
        parsed.to_string(),
        format!("plugin:{}:x", plugin_id.as_uuid().hyphenated())
    );
}

#[test]
fn action_ref_serializes_user_and_plugin_forms_as_strings() {
    let user = ActionRef::user("my-macro").expect("valid");
    assert_eq!(
        serde_json::to_string(&user).expect("serialize"),
        "\"user:my-macro\""
    );
    assert_eq!(String::from(user.clone()), "user:my-macro");

    let plugin_id = PluginId::new();
    let plugin = ActionRef::plugin(plugin_id, "diff").expect("valid");
    let expected = format!("plugin:{}:diff", plugin_id.as_uuid());
    assert_eq!(
        serde_json::to_string(&plugin).expect("serialize"),
        format!("\"{expected}\"")
    );
    assert_eq!(String::from(plugin), expected);
}

#[test]
fn action_namespace_wire_form_is_pinned() {
    use serde_json::json;

    assert_eq!(
        serde_json::to_value(ActionNamespace::Core).expect("serialize"),
        json!("Core")
    );
    assert_eq!(
        serde_json::to_value(ActionNamespace::User).expect("serialize"),
        json!("User")
    );
    let plugin_id = PluginId::new();
    assert_eq!(
        serde_json::to_value(ActionNamespace::Plugin(plugin_id)).expect("serialize"),
        json!({ "Plugin": plugin_id.as_uuid().to_string() })
    );
}

#[test]
fn action_status_serializes_in_kebab_case() {
    assert_eq!(
        serde_json::to_string(&ActionStatus::Available).expect("serialize"),
        "\"available\""
    );
    assert_eq!(
        serde_json::to_string(&ActionStatus::ComingSoon).expect("serialize"),
        "\"coming-soon\""
    );
    let decoded: ActionStatus = serde_json::from_str("\"coming-soon\"").expect("deserialize");
    assert_eq!(decoded, ActionStatus::ComingSoon);
    let rejected: Result<ActionStatus, _> = serde_json::from_str("\"ComingSoon\"");
    assert_eq!(
        rejected
            .expect_err("PascalCase is not the wire form")
            .to_string(),
        "unknown variant `ComingSoon`, expected `available` or `coming-soon` at line 1 column 12"
    );
}

#[test]
fn action_handler_ref_wire_form_is_pinned() {
    use serde_json::json;

    assert_eq!(
        serde_json::to_value(ActionHandlerRef::CoreCommand(CommandKind::NewPane))
            .expect("serialize"),
        json!({ "CoreCommand": "NewPane" })
    );
    let plugin_id = PluginId::new();
    assert_eq!(
        serde_json::to_value(ActionHandlerRef::PluginHostCall(plugin_id)).expect("serialize"),
        json!({ "PluginHostCall": plugin_id.as_uuid().to_string() })
    );
    assert_eq!(
        serde_json::to_value(ActionHandlerRef::Sequence(vec![
            ActionRef::core("lock").expect("valid"),
            ActionRef::core("new-tab").expect("valid"),
        ]))
        .expect("serialize"),
        json!({ "Sequence": ["core:lock", "core:new-tab"] })
    );
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
        continuous: false,
    };
    roundtrip(&metadata);
}

#[test]
fn action_metadata_continuous_is_false_when_absent_on_the_wire() {
    let metadata = ActionMetadata {
        namespace: ActionNamespace::Core,
        display_name: "Resize Pane".to_string(),
        description: "Grow or shrink the focused pane along one edge".to_string(),
        scope_class: ActionScope::PaneSession,
        target_compat: vec![TargetKind::Pane],
        args_schema: None,
        handler: ActionHandlerRef::CoreCommand(CommandKind::ResizePane),
        status: ActionStatus::Available,
        continuous: true,
    };
    let mut value = serde_json::to_value(&metadata).expect("serialize");
    assert_eq!(value["continuous"], serde_json::Value::Bool(true));
    let removed = value
        .as_object_mut()
        .expect("metadata is an object")
        .remove("continuous");
    assert_eq!(removed, Some(serde_json::Value::Bool(true)));

    let decoded: ActionMetadata = serde_json::from_value(value).expect("deserialize");
    assert_eq!(
        decoded,
        ActionMetadata {
            continuous: false,
            ..metadata
        }
    );
}

#[test]
#[should_panic(expected = "core seed action name must satisfy the action-name grammar")]
fn core_seed_panics_on_an_invalid_name() {
    let _ = core_seed(
        "Bad Name",
        "Bad",
        "An invalid seed",
        ActionScope::Global,
        vec![],
        ActionHandlerRef::CoreCommand(CommandKind::Quit),
        ActionStatus::Available,
    );
}

#[test]
fn mouse_select_seed_display_name_is_the_hint_label() {
    assert_eq!(MOUSE_SELECT_HINT, "Mouse Select");
    assert_eq!(MOUSE_UNSELECT_HINT, "Mouse Unselect");
    let seeds = core_action_seeds();
    let mouse_select = ActionRef::core("mouse-select").expect("valid");
    let (_, metadata) = seeds
        .iter()
        .find(|(action, _)| *action == mouse_select)
        .expect("mouse-select is seeded");
    assert_eq!(metadata.display_name, MOUSE_SELECT_HINT);
}

/// Pins every seed's position, command kind, scope, and targets, in table
/// order. `koshi actions list` prints the `Available` rows in this order.
#[test]
fn core_seed_order_kind_scope_and_targets_are_pinned() {
    use ActionScope::{Client, Global, PaneSession, Tab};
    use TargetKind::{Client as ClientTarget, Pane, Session, Tab as TabTarget};

    let seeds = core_action_seeds();
    for (_, metadata) in &seeds {
        assert_eq!(metadata.args_schema, None);
    }
    let actual: Vec<(String, ActionHandlerRef, ActionScope, Vec<TargetKind>)> = seeds
        .into_iter()
        .map(|(action, metadata)| {
            (
                action.to_string(),
                metadata.handler,
                metadata.scope_class,
                metadata.target_compat,
            )
        })
        .collect();

    let expected: Vec<(String, ActionHandlerRef, ActionScope, Vec<TargetKind>)> = [
        (
            "core:new-pane",
            CommandKind::NewPane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:new-pane-left",
            CommandKind::NewPane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:new-pane-down",
            CommandKind::NewPane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:new-pane-up",
            CommandKind::NewPane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:new-pane-right",
            CommandKind::NewPane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:new-pane-stacked",
            CommandKind::NewPane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:close-pane",
            CommandKind::ClosePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:close-pane-tree",
            CommandKind::ClosePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:resize-pane",
            CommandKind::ResizePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:resize-pane-left",
            CommandKind::ResizePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:resize-pane-down",
            CommandKind::ResizePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:resize-pane-up",
            CommandKind::ResizePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:resize-pane-right",
            CommandKind::ResizePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:focus-pane",
            CommandKind::FocusPane,
            Client,
            vec![Pane, ClientTarget],
        ),
        (
            "core:focus-pane-left",
            CommandKind::FocusPane,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:focus-pane-down",
            CommandKind::FocusPane,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:focus-pane-up",
            CommandKind::FocusPane,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:focus-pane-right",
            CommandKind::FocusPane,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:toggle-pane-fullscreen",
            CommandKind::TogglePaneFullscreen,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:write-to-pane",
            CommandKind::WriteToPane,
            PaneSession,
            vec![Pane],
        ),
        ("core:new-tab", CommandKind::NewTab, Tab, vec![TabTarget]),
        (
            "core:close-tab",
            CommandKind::CloseTab,
            Tab,
            vec![TabTarget],
        ),
        (
            "core:focus-tab",
            CommandKind::FocusTab,
            Client,
            vec![TabTarget, ClientTarget],
        ),
        (
            "core:next-tab",
            CommandKind::FocusTab,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:previous-tab",
            CommandKind::FocusTab,
            Client,
            vec![ClientTarget],
        ),
        ("core:move-tab", CommandKind::MoveTab, Tab, vec![TabTarget]),
        (
            "core:quit",
            CommandKind::Quit,
            Client,
            vec![ClientTarget, Session],
        ),
        (
            "core:toggle-lock",
            CommandKind::ToggleLockMode,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:lock",
            CommandKind::SetLockMode,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:unlock",
            CommandKind::SetLockMode,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:mouse-select",
            CommandKind::ToggleMouseSelect,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:run",
            CommandKind::RunCommandPane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:copy-selection",
            CommandKind::Visual,
            PaneSession,
            vec![Pane],
        ),
        ("core:plugin-install", CommandKind::Plugin, Global, vec![]),
        ("core:plugin-uninstall", CommandKind::Plugin, Global, vec![]),
        ("core:plugin-enable", CommandKind::Plugin, Global, vec![]),
        ("core:plugin-disable", CommandKind::Plugin, Global, vec![]),
        ("core:plugin-update", CommandKind::Plugin, Global, vec![]),
        ("core:plugin-reload", CommandKind::Plugin, Global, vec![]),
    ]
    .into_iter()
    .map(|(name, kind, scope, targets)| {
        (
            name.to_string(),
            ActionHandlerRef::CoreCommand(kind),
            scope,
            targets,
        )
    })
    .collect();

    assert_eq!(actual, expected);
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

/// Pins which seeds are coming-soon: `core:copy-selection` and the six plugin
/// actions have no runtime handler, so each is seeded `ComingSoon` and every
/// other action is `Available`.
#[test]
fn coming_soon_seeds_are_pinned() {
    let mut coming_soon: Vec<String> = core_action_seeds()
        .iter()
        .filter(|(_, metadata)| metadata.status == ActionStatus::ComingSoon)
        .map(|(action, _)| action.to_string())
        .collect();
    coming_soon.sort();

    // Visual mode contributes exactly one action — copying the highlight.
    // Entering and leaving it are not actions (a drag enters, any key leaves),
    // and setting/clearing the selection is the mouse layer's command, not a
    // name a user can bind.
    let mut expected = [
        "core:copy-selection",
        "core:plugin-disable",
        "core:plugin-enable",
        "core:plugin-install",
        "core:plugin-reload",
        "core:plugin-uninstall",
        "core:plugin-update",
    ]
    .map(String::from)
    .to_vec();
    expected.sort();

    assert_eq!(coming_soon, expected);
}

/// Pins which seeds are continuous: exactly the resize-pane and focus-pane
/// families. A new member of either family added without the `continuous`
/// flag — or the flag appearing on any other action — changes this list and
/// fails the assert.
#[test]
fn continuous_seeds_are_pinned() {
    let mut continuous: Vec<String> = core_action_seeds()
        .iter()
        .filter(|(_, metadata)| metadata.continuous)
        .map(|(action, _)| action.to_string())
        .collect();
    continuous.sort();

    let mut expected = [
        "core:resize-pane",
        "core:resize-pane-left",
        "core:resize-pane-down",
        "core:resize-pane-up",
        "core:resize-pane-right",
        "core:focus-pane",
        "core:focus-pane-left",
        "core:focus-pane-down",
        "core:focus-pane-up",
        "core:focus-pane-right",
    ]
    .map(String::from)
    .to_vec();
    expected.sort();

    assert_eq!(continuous, expected);
}

/// Pins the exact set of built-in actions. Adding, removing, or renaming a seed
/// changes this list and fails the assert.
#[test]
fn core_seed_snapshot_is_stable() {
    let mut names: Vec<String> = core_action_seeds()
        .iter()
        .map(|(action, _)| action.to_string())
        .collect();
    names.sort();

    let expected = vec![
        "core:close-pane",
        "core:close-pane-tree",
        "core:close-tab",
        "core:copy-selection",
        "core:focus-pane",
        "core:focus-pane-down",
        "core:focus-pane-left",
        "core:focus-pane-right",
        "core:focus-pane-up",
        "core:focus-tab",
        "core:lock",
        "core:mouse-select",
        "core:move-tab",
        "core:new-pane",
        "core:new-pane-down",
        "core:new-pane-left",
        "core:new-pane-right",
        "core:new-pane-stacked",
        "core:new-pane-up",
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
        "core:resize-pane",
        "core:resize-pane-down",
        "core:resize-pane-left",
        "core:resize-pane-right",
        "core:resize-pane-up",
        "core:run",
        "core:toggle-lock",
        "core:toggle-pane-fullscreen",
        "core:unlock",
        "core:write-to-pane",
    ];
    assert_eq!(names, expected);
}
