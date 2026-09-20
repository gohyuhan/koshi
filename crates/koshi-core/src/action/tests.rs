//! Tests for the action vocabulary.

use super::*;
use crate::ids::PluginId;
use std::collections::BTreeSet;

/// Roundtrip a value through JSON and assert it survives unchanged.
fn assert_json_roundtrip<Roundtrippable>(roundtrippable_value: &Roundtrippable)
where
    Roundtrippable: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let serialized_json = serde_json::to_string(roundtrippable_value).expect("serialize");
    let decoded_roundtrippable: Roundtrippable =
        serde_json::from_str(&serialized_json).expect("deserialize");
    assert_eq!(*roundtrippable_value, decoded_roundtrippable);
}

#[test]
fn action_name_parser_accepts_valid_grammar() {
    for action_name_text in [
        "a",
        "new-pane",
        "toggle-pane-fullscreen",
        "x9",
        "a-1-b-2",
        "a-",
        "a--b",
    ] {
        assert_eq!(
            ActionName::parse_action_name(action_name_text).map(String::from),
            Ok(action_name_text.to_string()),
            "{action_name_text:?} should be valid"
        );
    }
    // Exactly the maximum length (1 + 30) is allowed.
    let maximum_action_name = format!("a{}", "b".repeat(MAX_ACTION_NAME_CHARACTER_COUNT - 1));
    assert_eq!(maximum_action_name.len(), MAX_ACTION_NAME_CHARACTER_COUNT);
    assert_eq!(
        ActionName::parse_action_name(&maximum_action_name).map(String::from),
        Ok(maximum_action_name.clone())
    );
}

#[test]
fn action_name_parser_rejects_non_ascii_characters() {
    assert_eq!(
        ActionName::parse_action_name("é"),
        Err(ActionNameError::InvalidStart {
            invalid_character: 'é',
        })
    );
    assert_eq!(
        ActionName::parse_action_name("aé"),
        Err(ActionNameError::InvalidChar {
            invalid_character: 'é',
        })
    );
    assert_eq!(
        ActionName::parse_action_name("a😀"),
        Err(ActionNameError::InvalidChar {
            invalid_character: '😀',
        })
    );
    assert_eq!(
        ActionName::parse_action_name("a b"),
        Err(ActionNameError::InvalidChar {
            invalid_character: ' ',
        })
    );
}

#[test]
fn action_name_error_display_uses_stable_messages() {
    assert_eq!(ActionNameError::Empty.to_string(), "action name is empty");
    assert_eq!(
        ActionNameError::TooLong {
            character_count: 32,
        }
        .to_string(),
        "action name is 32 chars; the maximum is 31"
    );
    assert_eq!(
        ActionNameError::InvalidStart {
            invalid_character: 'N',
        }
        .to_string(),
        "action name must start with a lowercase letter, found 'N'"
    );
    assert_eq!(
        ActionNameError::InvalidChar {
            invalid_character: '_',
        }
        .to_string(),
        "action name may only contain [a-z0-9-], found '_'"
    );
}

#[test]
fn action_name_display_and_serialization_preserve_input_string() {
    let action_name = ActionName::parse_action_name("focus-pane").expect("valid");
    assert_eq!(action_name.get_name(), "focus-pane");
    assert_eq!(action_name.to_string(), "focus-pane");
    assert_eq!(String::from(action_name.clone()), "focus-pane");
    assert_eq!(
        serde_json::to_string(&action_name).expect("serialize"),
        "\"focus-pane\""
    );
}

#[test]
fn action_name_parser_rejects_invalid_grammar() {
    assert_eq!(
        ActionName::parse_action_name(""),
        Err(ActionNameError::Empty)
    );
    assert_eq!(
        ActionName::parse_action_name("New"),
        Err(ActionNameError::InvalidStart {
            invalid_character: 'N',
        })
    );
    assert_eq!(
        ActionName::parse_action_name("1pane"),
        Err(ActionNameError::InvalidStart {
            invalid_character: '1',
        })
    );
    assert_eq!(
        ActionName::parse_action_name("-pane"),
        Err(ActionNameError::InvalidStart {
            invalid_character: '-',
        })
    );
    assert_eq!(
        ActionName::parse_action_name("new_pane"),
        Err(ActionNameError::InvalidChar {
            invalid_character: '_',
        })
    );
    assert_eq!(
        ActionName::parse_action_name("newPane"),
        Err(ActionNameError::InvalidChar {
            invalid_character: 'P',
        })
    );
    let overlong_action_name = format!("a{}", "b".repeat(MAX_ACTION_NAME_CHARACTER_COUNT));
    assert_eq!(
        ActionName::parse_action_name(&overlong_action_name),
        Err(ActionNameError::TooLong {
            character_count: MAX_ACTION_NAME_CHARACTER_COUNT + 1
        })
    );
}

#[test]
fn action_name_parser_reports_invalid_character_before_length_error() {
    // The char-grammar scan runs over every character before the length
    // check, so a bad character anywhere — even past the length cap — wins
    // over `TooLong`, per the documented precedence.
    let action_name_text = format!("a{}_", "b".repeat(40));
    assert!(action_name_text.chars().count() > MAX_ACTION_NAME_CHARACTER_COUNT);
    assert_eq!(
        ActionName::parse_action_name(&action_name_text),
        Err(ActionNameError::InvalidChar {
            invalid_character: '_'
        })
    );
}

#[test]
fn action_name_deserialization_validates_grammar() {
    assert_json_roundtrip(&ActionName::parse_action_name("focus-pane").expect("valid"));
    let decoded_action_name: Result<ActionName, _> = serde_json::from_str("\"BadName\"");
    assert_eq!(
        decoded_action_name
            .expect_err("invalid name must not deserialize")
            .to_string(),
        "action name must start with a lowercase letter, found 'B'"
    );
}

#[test]
fn action_reference_display_includes_each_namespace_form() {
    let core_action_reference = ActionReference::from_core_action_name("new-pane").expect("valid");
    assert_eq!(core_action_reference.to_string(), "core:new-pane");

    let user_action_reference = ActionReference::from_user_action_name("my-macro").expect("valid");
    assert_eq!(user_action_reference.to_string(), "user:my-macro");

    let plugin_id = PluginId::new();
    let plugin_action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "open-status").expect("valid");
    assert_eq!(
        plugin_action_reference.to_string(),
        format!("plugin:{}:open-status", plugin_id.get_uuid())
    );
}

#[test]
fn action_reference_roundtrips_each_namespace_through_serde() {
    assert_json_roundtrip(&ActionReference::from_core_action_name("close-pane").expect("valid"));
    assert_json_roundtrip(&ActionReference::from_user_action_name("workflow-1").expect("valid"));
    assert_json_roundtrip(
        &ActionReference::from_plugin_action_name(PluginId::new(), "diff").expect("valid"),
    );
}

#[test]
fn action_reference_serialization_uses_canonical_string() {
    // The wire form is the documented `core:new-pane` token, not a struct, so a
    // keymap referencing actions by name decodes straight into an `ActionReference`.
    let core_action_reference = ActionReference::from_core_action_name("new-pane").expect("valid");
    assert_eq!(
        serde_json::to_string(&core_action_reference).expect("serialize"),
        "\"core:new-pane\""
    );

    let decoded_action_reference: ActionReference =
        serde_json::from_str("\"core:new-pane\"").expect("deserialize");
    assert_eq!(decoded_action_reference, core_action_reference);
}

#[test]
fn action_reference_parser_accepts_canonical_strings() {
    assert_eq!(
        "core:new-pane".parse::<ActionReference>().expect("valid"),
        ActionReference::from_core_action_name("new-pane").expect("valid")
    );
    assert_eq!(
        "user:my-macro".parse::<ActionReference>().expect("valid"),
        ActionReference::from_user_action_name("my-macro").expect("valid")
    );

    let plugin_id = PluginId::new();
    let plugin_action_reference_text = format!("plugin:{}:open-status", plugin_id.get_uuid());
    assert_eq!(
        plugin_action_reference_text
            .parse::<ActionReference>()
            .expect("valid"),
        ActionReference::from_plugin_action_name(plugin_id, "open-status").expect("valid")
    );
}

#[test]
fn action_reference_parser_rejects_malformed_strings() {
    assert_eq!(
        "new-pane".parse::<ActionReference>(),
        Err(ActionReferenceParseError::MissingNamespace)
    );
    assert_eq!(
        "shell:new-pane".parse::<ActionReference>(),
        Err(ActionReferenceParseError::UnknownNamespace {
            unknown_namespace: "shell".to_string()
        })
    );
    assert_eq!(
        "plugin:not-a-uuid:x".parse::<ActionReference>(),
        Err(ActionReferenceParseError::InvalidPluginId)
    );
    assert_eq!(
        format!("plugin:{}", PluginId::new().get_uuid()).parse::<ActionReference>(),
        Err(ActionReferenceParseError::MissingPluginName)
    );
    assert_eq!(
        "core:Bad Name".parse::<ActionReference>(),
        Err(ActionReferenceParseError::InvalidActionName(
            ActionNameError::InvalidStart {
                invalid_character: 'B'
            }
        ))
    );

    // The same rejection holds when decoding from the wire.
    let decoded_action_reference: Result<ActionReference, _> =
        serde_json::from_str("\"core:Bad Name\"");
    assert_eq!(
        decoded_action_reference
            .expect_err("invalid action name must not deserialize")
            .to_string(),
        "action name must start with a lowercase letter, found 'B'"
    );
}

#[test]
fn action_reference_parser_reports_first_failing_rule() {
    let plugin_id = PluginId::new();
    let uuid = plugin_id.get_uuid();
    let parse_error_cases: &[(String, ActionReferenceParseError)] = &[
        (String::new(), ActionReferenceParseError::MissingNamespace),
        (
            "core".to_string(),
            ActionReferenceParseError::MissingNamespace,
        ),
        (
            ":".to_string(),
            ActionReferenceParseError::UnknownNamespace {
                unknown_namespace: String::new(),
            },
        ),
        (
            "CORE:new-pane".to_string(),
            ActionReferenceParseError::UnknownNamespace {
                unknown_namespace: "CORE".to_string(),
            },
        ),
        (
            " core:new-pane".to_string(),
            ActionReferenceParseError::UnknownNamespace {
                unknown_namespace: " core".to_string(),
            },
        ),
        (
            "core:".to_string(),
            ActionReferenceParseError::InvalidActionName(ActionNameError::Empty),
        ),
        (
            "user:".to_string(),
            ActionReferenceParseError::InvalidActionName(ActionNameError::Empty),
        ),
        (
            "core:new-pane:x".to_string(),
            ActionReferenceParseError::InvalidActionName(ActionNameError::InvalidChar {
                invalid_character: ':',
            }),
        ),
        (
            "plugin:".to_string(),
            ActionReferenceParseError::MissingPluginName,
        ),
        (
            "plugin:not-a-uuid".to_string(),
            ActionReferenceParseError::MissingPluginName,
        ),
        (
            "plugin::x".to_string(),
            ActionReferenceParseError::InvalidPluginId,
        ),
        (
            format!("plugin:{uuid}:"),
            ActionReferenceParseError::InvalidActionName(ActionNameError::Empty),
        ),
        (
            format!("plugin:{uuid}:a:b"),
            ActionReferenceParseError::InvalidActionName(ActionNameError::InvalidChar {
                invalid_character: ':',
            }),
        ),
    ];
    for (action_reference_text, expected_parse_error) in parse_error_cases {
        assert_eq!(
            action_reference_text.parse::<ActionReference>(),
            Err(expected_parse_error.clone()),
            "for {action_reference_text:?}"
        );
    }
}

#[test]
fn action_reference_parse_error_display_uses_stable_messages() {
    assert_eq!(
        ActionReferenceParseError::MissingNamespace.to_string(),
        "action reference is missing a 'namespace:' prefix"
    );
    assert_eq!(
        ActionReferenceParseError::UnknownNamespace {
            unknown_namespace: "shell".to_string()
        }
        .to_string(),
        "unknown action namespace \"shell\"; expected core, plugin, or user"
    );
    assert_eq!(
        ActionReferenceParseError::MissingPluginName.to_string(),
        "plugin action reference must be 'plugin:<uuid>:<name>'"
    );
    assert_eq!(
        ActionReferenceParseError::InvalidPluginId.to_string(),
        "plugin action reference has an invalid UUID"
    );
    assert_eq!(
        ActionReferenceParseError::InvalidActionName(ActionNameError::Empty).to_string(),
        "action name is empty"
    );
}

#[test]
fn action_reference_parse_error_source_exposes_only_name_error() {
    use std::error::Error;

    let name_error = ActionReferenceParseError::InvalidActionName(ActionNameError::Empty);
    assert_eq!(
        name_error.source().map(ToString::to_string),
        Some("action name is empty".to_string())
    );
    for action_parse_error in [
        ActionReferenceParseError::MissingNamespace,
        ActionReferenceParseError::UnknownNamespace {
            unknown_namespace: "shell".to_string(),
        },
        ActionReferenceParseError::MissingPluginName,
        ActionReferenceParseError::InvalidPluginId,
    ] {
        assert_eq!(
            action_parse_error.source().map(ToString::to_string),
            None,
            "for {action_parse_error:?}"
        );
    }
}

#[test]
fn action_reference_parser_accepts_plugin_uuid_without_hyphens() {
    let plugin_id = PluginId::new();
    let plugin_action_reference_text = format!("plugin:{}:x", plugin_id.get_uuid().simple());
    let parsed_action_reference = plugin_action_reference_text
        .parse::<ActionReference>()
        .expect("valid");
    assert_eq!(
        parsed_action_reference,
        ActionReference::from_plugin_action_name(plugin_id, "x").expect("valid")
    );
    // The canonical form always prints the hyphenated UUID.
    assert_eq!(
        parsed_action_reference.to_string(),
        format!("plugin:{}:x", plugin_id.get_uuid().hyphenated())
    );
}

#[test]
fn action_reference_serialization_uses_user_and_plugin_strings() {
    let user_action_reference = ActionReference::from_user_action_name("my-macro").expect("valid");
    assert_eq!(
        serde_json::to_string(&user_action_reference).expect("serialize"),
        "\"user:my-macro\""
    );
    assert_eq!(String::from(user_action_reference.clone()), "user:my-macro");

    let plugin_id = PluginId::new();
    let plugin_action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "diff").expect("valid");
    let expected_plugin_reference = format!("plugin:{}:diff", plugin_id.get_uuid());
    assert_eq!(
        serde_json::to_string(&plugin_action_reference).expect("serialize"),
        format!("\"{expected_plugin_reference}\"")
    );
    assert_eq!(
        String::from(plugin_action_reference),
        expected_plugin_reference
    );
}

#[test]
fn action_namespace_serialization_uses_stable_wire_forms() {
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
        json!({ "Plugin": plugin_id.get_uuid().to_string() })
    );
}

#[test]
fn action_status_serialization_uses_declared_variant_names() {
    assert_eq!(
        serde_json::to_string(&ActionStatus::Available).expect("serialize"),
        "\"Available\""
    );
    assert_eq!(
        serde_json::to_string(&ActionStatus::ComingSoon).expect("serialize"),
        "\"ComingSoon\""
    );
    let decoded_action_status: ActionStatus =
        serde_json::from_str("\"ComingSoon\"").expect("deserialize");
    assert_eq!(decoded_action_status, ActionStatus::ComingSoon);
    let rejected: Result<ActionStatus, _> = serde_json::from_str("\"coming-soon\"");
    assert_eq!(
        rejected
            .expect_err("kebab-case is not the wire form")
            .to_string(),
        "unknown variant `coming-soon`, expected `Available` or `ComingSoon` at line 1 column 13"
    );
}

#[test]
fn action_handler_reference_serialization_uses_stable_wire_forms() {
    use serde_json::json;

    assert_eq!(
        serde_json::to_value(ActionHandlerReference::CoreCommand(CommandKind::NewPane))
            .expect("serialize"),
        json!({ "CoreCommand": "NewPane" })
    );
    let plugin_id = PluginId::new();
    assert_eq!(
        serde_json::to_value(ActionHandlerReference::PluginHostCall(plugin_id)).expect("serialize"),
        json!({ "PluginHostCall": plugin_id.get_uuid().to_string() })
    );
    assert_eq!(
        serde_json::to_value(ActionHandlerReference::Sequence(vec![
            ActionReference::from_core_action_name("lock").expect("valid"),
            ActionReference::from_core_action_name("new-tab").expect("valid"),
        ]))
        .expect("serialize"),
        json!({ "Sequence": ["core:lock", "core:new-tab"] })
    );
}

#[test]
fn action_handler_reference_roundtrips_through_serde() {
    assert_json_roundtrip(&ActionHandlerReference::CoreCommand(CommandKind::NewPane));
    assert_json_roundtrip(&ActionHandlerReference::PluginHostCall(PluginId::new()));
    assert_json_roundtrip(&ActionHandlerReference::Sequence(vec![
        ActionReference::from_core_action_name("lock").expect("valid"),
        ActionReference::from_core_action_name("new-tab").expect("valid"),
    ]));
}

#[test]
fn action_metadata_roundtrips_through_serde() {
    let metadata = ActionMetadata {
        namespace: ActionNamespace::Core,
        display_name: "New Pane".to_string(),
        description: "Split the focused pane".to_string(),
        scope: ActionScope::PaneSession,
        target_kinds: vec![TargetKind::Pane],
        handler: ActionHandlerReference::CoreCommand(CommandKind::NewPane),
        action_status: ActionStatus::Available,
        is_continuous: false,
    };
    assert_json_roundtrip(&metadata);
}

#[test]
fn action_metadata_defaults_is_continuous_when_wire_field_is_absent() {
    let metadata = ActionMetadata {
        namespace: ActionNamespace::Core,
        display_name: "Resize Pane".to_string(),
        description: "Grow or shrink the focused pane along one edge".to_string(),
        scope: ActionScope::PaneSession,
        target_kinds: vec![TargetKind::Pane],
        handler: ActionHandlerReference::CoreCommand(CommandKind::ResizePane),
        action_status: ActionStatus::Available,
        is_continuous: true,
    };
    let mut metadata_json = serde_json::to_value(&metadata).expect("serialize");
    assert_eq!(
        metadata_json["is_continuous"],
        serde_json::Value::Bool(true)
    );
    let removed_continuous_wire_field = metadata_json
        .as_object_mut()
        .expect("metadata is an object")
        .remove("is_continuous");
    assert_eq!(
        removed_continuous_wire_field,
        Some(serde_json::Value::Bool(true))
    );

    let decoded_metadata: ActionMetadata =
        serde_json::from_value(metadata_json).expect("deserialize");
    assert_eq!(
        decoded_metadata,
        ActionMetadata {
            is_continuous: false,
            ..metadata
        }
    );
}

#[test]
#[should_panic(expected = "core seed action name must satisfy the action-name grammar")]
fn core_action_seed_panics_on_invalid_action_name() {
    let _ = build_core_action_seed(
        "Bad Name",
        "Bad",
        "An invalid seed",
        ActionScope::Global,
        vec![],
        ActionHandlerReference::CoreCommand(CommandKind::Quit),
        ActionStatus::Available,
    );
}

#[test]
fn mouse_select_seed_uses_hint_label_as_display_name() {
    assert_eq!(MOUSE_SELECT_HINT, "Mouse Select");
    assert_eq!(MOUSE_UNSELECT_HINT, "Mouse Unselect");
    let seeds = build_core_action_seeds();
    let mouse_select_action_reference =
        ActionReference::from_core_action_name("mouse-select").expect("valid");
    let (_, mouse_select_metadata) = seeds
        .iter()
        .find(|(action_reference, _)| *action_reference == mouse_select_action_reference)
        .expect("mouse-select is seeded");
    assert_eq!(mouse_select_metadata.display_name, MOUSE_SELECT_HINT);
}

/// Pins every seed's position, command kind, scope, and targets, in table
/// order. `koshi actions list` prints the `Available` rows in this order.
#[test]
fn core_action_seed_order_kind_scope_and_targets_are_stable() {
    use ActionScope::{Client, Global, PaneSession, Tab};
    use TargetKind::{Client as ClientTarget, Pane, Session, Tab as TabTarget};

    let seeds = build_core_action_seeds();
    let actual_seed_metadata: Vec<(String, ActionHandlerReference, ActionScope, Vec<TargetKind>)> =
        seeds
            .into_iter()
            .map(|(action, metadata)| {
                (
                    action.to_string(),
                    metadata.handler,
                    metadata.scope,
                    metadata.target_kinds,
                )
            })
            .collect();

    let expected_seed_metadata: Vec<(
        String,
        ActionHandlerReference,
        ActionScope,
        Vec<TargetKind>,
    )> = [
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
            "core:move-pane",
            CommandKind::MovePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:move-pane-left",
            CommandKind::MovePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:move-pane-down",
            CommandKind::MovePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:move-pane-up",
            CommandKind::MovePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:move-pane-right",
            CommandKind::MovePane,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:swap-panes",
            CommandKind::SwapPanes,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:place-pane",
            CommandKind::PlacePane,
            PaneSession,
            vec![Pane, TabTarget],
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
            "core:scroll-pane",
            CommandKind::ScrollPane,
            Client,
            vec![ClientTarget, Pane],
        ),
        (
            "core:scroll-pane-up",
            CommandKind::ScrollPane,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:scroll-pane-down",
            CommandKind::ScrollPane,
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
    .map(|(action_name, command_kind, action_scope, target_kinds)| {
        (
            action_name.to_string(),
            ActionHandlerReference::CoreCommand(command_kind),
            action_scope,
            target_kinds,
        )
    })
    .collect();

    assert_eq!(actual_seed_metadata, expected_seed_metadata);
}

#[test]
fn core_action_seeds_have_valid_namespaces_and_serde_forms() {
    let seeds = build_core_action_seeds();

    // Every seed is in the core namespace, on both the ref and its metadata.
    for (action_reference, action_metadata) in &seeds {
        assert_eq!(action_reference.namespace, ActionNamespace::Core);
        assert_eq!(action_metadata.namespace, ActionNamespace::Core);
    }

    // No duplicate action references.
    let unique_action_references: BTreeSet<String> = seeds
        .iter()
        .map(|(action_reference, _)| action_reference.to_string())
        .collect();
    assert_eq!(
        unique_action_references.len(),
        seeds.len(),
        "seed action names must be unique"
    );

    // The whole table roundtrips through serde.
    for (action_reference, action_metadata) in &seeds {
        assert_json_roundtrip(action_reference);
        assert_json_roundtrip(action_metadata);
    }
}

/// Pins the client-scoped seeds: lock mode, focus, and scroll are per-client
/// state, so their actions carry the `Client` scope and accept a client target.
#[test]
fn lock_and_focus_seeds_use_client_scope_and_targets() {
    let seeds = build_core_action_seeds();
    let get_action_metadata = |action_name: &str| {
        let action_reference =
            ActionReference::from_core_action_name(action_name).expect("valid seed name");
        seeds
            .iter()
            .find(|(seeded_action_reference, _)| *seeded_action_reference == action_reference)
            .unwrap_or_else(|| panic!("{action_name} must be seeded"))
            .1
            .clone()
    };

    let client_scoped_action_cases: &[(&str, Vec<TargetKind>)] = &[
        ("focus-pane", vec![TargetKind::Pane, TargetKind::Client]),
        ("focus-tab", vec![TargetKind::Tab, TargetKind::Client]),
        ("next-tab", vec![TargetKind::Client]),
        ("previous-tab", vec![TargetKind::Client]),
        ("lock", vec![TargetKind::Client]),
        ("unlock", vec![TargetKind::Client]),
        ("toggle-lock", vec![TargetKind::Client]),
        ("scroll-pane", vec![TargetKind::Client, TargetKind::Pane]),
        ("scroll-pane-up", vec![TargetKind::Client]),
        ("scroll-pane-down", vec![TargetKind::Client]),
    ];
    for (action_name, target_kinds) in client_scoped_action_cases {
        let action_metadata = get_action_metadata(action_name);
        assert_eq!(
            action_metadata.scope,
            ActionScope::Client,
            "for {action_name}"
        );
        assert_eq!(
            action_metadata.target_kinds, *target_kinds,
            "for {action_name}"
        );
    }
}

/// Pins which seeds are coming-soon: `core:copy-selection` and the six plugin
/// actions have no runtime handler, so each is seeded `ComingSoon` and every
/// other action is `Available`.
#[test]
fn coming_soon_action_seeds_are_stable() {
    let mut coming_soon: Vec<String> = build_core_action_seeds()
        .iter()
        .filter(|(_, action_metadata)| action_metadata.action_status == ActionStatus::ComingSoon)
        .map(|(action_reference, _)| action_reference.to_string())
        .collect();
    coming_soon.sort();

    // Visual mode contributes exactly one action — copying the highlight.
    // Entering and leaving it are not actions (a drag enters, any key leaves),
    // and setting/clearing the selection is the mouse layer's command, not a
    // name a user can bind.
    let mut expected_coming_soon_action_names = [
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
    expected_coming_soon_action_names.sort();

    assert_eq!(coming_soon, expected_coming_soon_action_names);
}

/// Pins which seeds are continuous: the resize-pane, focus-pane, and scroll
/// action families. A new member of a family added without the `continuous`
/// flag — or the flag appearing on any other action — changes this list and
/// fails the assert.
#[test]
fn continuous_action_seeds_are_stable() {
    let mut continuous: Vec<String> = build_core_action_seeds()
        .iter()
        .filter(|(_, action_metadata)| action_metadata.is_continuous)
        .map(|(action_reference, _)| action_reference.to_string())
        .collect();
    continuous.sort();

    let mut expected_continuous_action_names = [
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
        "core:scroll-pane-down",
        "core:scroll-pane-up",
    ]
    .map(String::from)
    .to_vec();
    expected_continuous_action_names.sort();

    assert_eq!(continuous, expected_continuous_action_names);
}

/// Pins the exact set of built-in actions. Adding, removing, or renaming a seed
/// changes this list and fails the assert.
#[test]
fn core_action_seed_name_snapshot_is_stable() {
    let mut action_names: Vec<String> = build_core_action_seeds()
        .iter()
        .map(|(action_reference, _)| action_reference.to_string())
        .collect();
    action_names.sort();

    let expected_action_names = vec![
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
        "core:move-pane",
        "core:move-pane-down",
        "core:move-pane-left",
        "core:move-pane-right",
        "core:move-pane-up",
        "core:move-tab",
        "core:new-pane",
        "core:new-pane-down",
        "core:new-pane-left",
        "core:new-pane-right",
        "core:new-pane-stacked",
        "core:new-pane-up",
        "core:new-tab",
        "core:next-tab",
        "core:place-pane",
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
        "core:scroll-pane",
        "core:scroll-pane-down",
        "core:scroll-pane-up",
        "core:swap-panes",
        "core:toggle-lock",
        "core:toggle-pane-fullscreen",
        "core:unlock",
        "core:write-to-pane",
    ];
    assert_eq!(action_names, expected_action_names);
}
