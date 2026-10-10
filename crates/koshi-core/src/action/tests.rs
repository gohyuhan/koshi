//! Tests for the action vocabulary.

use super::*;
use std::collections::BTreeSet;

/// Roundtrip an action representation through JSON and assert it survives unchanged.
fn assert_json_roundtrip<ActionRepresentation>(action_representation: &ActionRepresentation)
where
    ActionRepresentation:
        serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let serialized_json = serde_json::to_string(action_representation).expect("serialize");
    let decoded_action_representation: ActionRepresentation =
        serde_json::from_str(&serialized_json).expect("deserialize");
    assert_eq!(*action_representation, decoded_action_representation);
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
    let maximum_length_action_name =
        format!("a{}", "b".repeat(MAX_ACTION_NAME_CHARACTER_COUNT - 1));
    assert_eq!(
        maximum_length_action_name.len(),
        MAX_ACTION_NAME_CHARACTER_COUNT
    );
    assert_eq!(
        ActionName::parse_action_name(&maximum_length_action_name).map(String::from),
        Ok(maximum_length_action_name.clone())
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
fn action_reference_display_writes_the_core_prefix() {
    let core_action_reference = ActionReference::from_core_action_name("new-pane").expect("valid");
    assert_eq!(core_action_reference.to_string(), "core:new-pane");
    assert_eq!(String::from(core_action_reference), "core:new-pane");
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
    assert_json_roundtrip(&core_action_reference);
}

#[test]
fn action_reference_parser_accepts_canonical_strings() {
    assert_eq!(
        "core:new-pane".parse::<ActionReference>().expect("valid"),
        ActionReference::from_core_action_name("new-pane").expect("valid")
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

/// `user:` and `plugin:` are not namespaces: both are refused by name, like
/// any other unknown prefix.
#[test]
fn action_reference_parser_refuses_user_and_plugin_prefixes() {
    assert_eq!(
        "user:my-macro".parse::<ActionReference>(),
        Err(ActionReferenceParseError::UnknownNamespace {
            unknown_namespace: "user".to_string()
        })
    );
    assert_eq!(
        "plugin:0192f0c1-0000-7000-8000-000000000000:open-status".parse::<ActionReference>(),
        Err(ActionReferenceParseError::UnknownNamespace {
            unknown_namespace: "plugin".to_string()
        })
    );
}

#[test]
fn action_reference_parser_reports_first_failing_rule() {
    let action_reference_parse_error_cases: &[(&str, ActionReferenceParseError)] = &[
        ("", ActionReferenceParseError::MissingNamespace),
        ("core", ActionReferenceParseError::MissingNamespace),
        (
            ":",
            ActionReferenceParseError::UnknownNamespace {
                unknown_namespace: String::new(),
            },
        ),
        (
            "CORE:new-pane",
            ActionReferenceParseError::UnknownNamespace {
                unknown_namespace: "CORE".to_string(),
            },
        ),
        (
            " core:new-pane",
            ActionReferenceParseError::UnknownNamespace {
                unknown_namespace: " core".to_string(),
            },
        ),
        (
            "core:",
            ActionReferenceParseError::InvalidActionName(ActionNameError::Empty),
        ),
        (
            "core:new-pane:x",
            ActionReferenceParseError::InvalidActionName(ActionNameError::InvalidChar {
                invalid_character: ':',
            }),
        ),
    ];
    for (action_reference_text, expected_action_reference_parse_error) in
        action_reference_parse_error_cases
    {
        assert_eq!(
            action_reference_text.parse::<ActionReference>(),
            Err(expected_action_reference_parse_error.clone()),
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
        "unknown action namespace \"shell\"; expected core"
    );
    assert_eq!(
        ActionReferenceParseError::InvalidActionName(ActionNameError::Empty).to_string(),
        "action name is empty"
    );
}

#[test]
fn action_reference_parse_error_source_exposes_only_name_error() {
    use std::error::Error;

    let action_name_error = ActionReferenceParseError::InvalidActionName(ActionNameError::Empty);
    assert_eq!(
        action_name_error.source().map(ToString::to_string),
        Some("action name is empty".to_string())
    );
    for action_reference_parse_error in [
        ActionReferenceParseError::MissingNamespace,
        ActionReferenceParseError::UnknownNamespace {
            unknown_namespace: "shell".to_string(),
        },
    ] {
        assert_eq!(
            action_reference_parse_error
                .source()
                .map(ToString::to_string),
            None,
            "for {action_reference_parse_error:?}"
        );
    }
}

#[test]
#[should_panic(expected = "core seed action name must satisfy the action-name grammar")]
fn core_action_seed_panics_on_invalid_action_name() {
    let _ = build_core_action_seed(
        "Bad Name",
        "Bad",
        "An invalid seed",
        ActionScope::Client,
        vec![],
        ActionHandlerReference::CoreCommand(CommandKind::Quit),
    );
}

#[test]
fn mouse_select_seed_uses_hint_label_as_display_name() {
    assert_eq!(MOUSE_SELECT_HINT, "Mouse Select");
    assert_eq!(MOUSE_UNSELECT_HINT, "Mouse Unselect");
    let core_action_seeds = build_core_action_seeds();
    let mouse_select_action_reference =
        ActionReference::from_core_action_name("mouse-select").expect("valid");
    let (_, mouse_select_action_metadata) = core_action_seeds
        .iter()
        .find(|(action_reference, _)| *action_reference == mouse_select_action_reference)
        .expect("mouse-select is seeded");
    assert_eq!(mouse_select_action_metadata.display_name, MOUSE_SELECT_HINT);
}

/// Pins every seed's position, command kind, scope, and targets, in table
/// order. `koshi actions list` prints the rows in this order.
#[test]
fn core_action_seed_order_kind_scope_and_targets_are_stable() {
    use ActionScope::{Client, PaneSession, Tab};
    use TargetKind::{Client as ClientTarget, Pane, Session, Tab as TabTarget};

    let core_action_seeds = build_core_action_seeds();
    let actual_command_backed_action_metadata: Vec<(
        String,
        ActionHandlerReference,
        ActionScope,
        Vec<TargetKind>,
    )> = core_action_seeds
        .into_iter()
        .filter_map(
            |(action_reference, action_metadata)| match action_metadata.handler {
                ActionHandlerReference::CoreClient(_) => None,
                action_handler => Some((
                    action_reference.to_string(),
                    action_handler,
                    action_metadata.scope,
                    action_metadata.target_kinds,
                )),
            },
        )
        .collect();

    let expected_command_backed_action_metadata: Vec<(
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
        ("core:run", CommandKind::NewPane, PaneSession, vec![Pane]),
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

    assert_eq!(
        actual_command_backed_action_metadata,
        expected_command_backed_action_metadata
    );

    let actual_client_action_metadata: Vec<(
        String,
        ClientActionKind,
        ActionScope,
        Vec<TargetKind>,
    )> = build_core_action_seeds()
        .into_iter()
        .filter_map(
            |(action_reference, action_metadata)| match action_metadata.handler {
                ActionHandlerReference::CoreClient(client_action_kind) => Some((
                    action_reference.to_string(),
                    client_action_kind,
                    action_metadata.scope,
                    action_metadata.target_kinds,
                )),
                _ => None,
            },
        )
        .collect();
    let expected_client_action_metadata = vec![
        (
            "core:begin-pane-placement".to_string(),
            ClientActionKind::BeginPanePlacement,
            PaneSession,
            vec![Pane],
        ),
        (
            "core:select-pane-target-left".to_string(),
            ClientActionKind::SelectPaneTarget(Direction::Left),
            Client,
            vec![ClientTarget],
        ),
        (
            "core:select-pane-target-down".to_string(),
            ClientActionKind::SelectPaneTarget(Direction::Down),
            Client,
            vec![ClientTarget],
        ),
        (
            "core:select-pane-target-up".to_string(),
            ClientActionKind::SelectPaneTarget(Direction::Up),
            Client,
            vec![ClientTarget],
        ),
        (
            "core:select-pane-target-right".to_string(),
            ClientActionKind::SelectPaneTarget(Direction::Right),
            Client,
            vec![ClientTarget],
        ),
        (
            "core:select-pane-insertion-left".to_string(),
            ClientActionKind::SelectPaneInsertion(Direction::Left),
            Client,
            vec![ClientTarget],
        ),
        (
            "core:select-pane-insertion-down".to_string(),
            ClientActionKind::SelectPaneInsertion(Direction::Down),
            Client,
            vec![ClientTarget],
        ),
        (
            "core:select-pane-insertion-up".to_string(),
            ClientActionKind::SelectPaneInsertion(Direction::Up),
            Client,
            vec![ClientTarget],
        ),
        (
            "core:select-pane-insertion-right".to_string(),
            ClientActionKind::SelectPaneInsertion(Direction::Right),
            Client,
            vec![ClientTarget],
        ),
        (
            "core:cycle-pane-placement-span".to_string(),
            ClientActionKind::CyclePanePlacementSpan,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:select-next-placement-tab".to_string(),
            ClientActionKind::SelectNextPlacementTab,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:select-previous-placement-tab".to_string(),
            ClientActionKind::SelectPreviousPlacementTab,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:confirm-pane-placement".to_string(),
            ClientActionKind::ConfirmPanePlacement,
            Client,
            vec![ClientTarget],
        ),
        (
            "core:cancel-pane-placement".to_string(),
            ClientActionKind::CancelPanePlacement,
            Client,
            vec![ClientTarget],
        ),
    ];
    assert_eq!(
        actual_client_action_metadata,
        expected_client_action_metadata
    );
}

#[test]
fn core_action_seeds_are_unique_and_roundtrip_through_serde() {
    let core_action_seeds = build_core_action_seeds();

    // No duplicate action references.
    let unique_action_references: BTreeSet<String> = core_action_seeds
        .iter()
        .map(|(action_reference, _)| action_reference.to_string())
        .collect();
    assert_eq!(
        unique_action_references.len(),
        core_action_seeds.len(),
        "seed action names must be unique"
    );

    // Every seeded reference roundtrips through serde.
    for (action_reference, _) in &core_action_seeds {
        assert_json_roundtrip(action_reference);
    }
}

/// Pins the client-scoped seeds: lock mode, focus, and scroll are per-client
/// state, so their actions carry the `Client` scope and accept a client target.
#[test]
fn lock_and_focus_seeds_use_client_scope_and_targets() {
    let core_action_seeds = build_core_action_seeds();
    let find_action_metadata = |action_name: &str| {
        let action_reference =
            ActionReference::from_core_action_name(action_name).expect("valid seed name");
        core_action_seeds
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
        ("select-pane-target-left", vec![TargetKind::Client]),
        ("select-pane-target-down", vec![TargetKind::Client]),
        ("select-pane-target-up", vec![TargetKind::Client]),
        ("select-pane-target-right", vec![TargetKind::Client]),
        ("select-pane-insertion-left", vec![TargetKind::Client]),
        ("select-pane-insertion-down", vec![TargetKind::Client]),
        ("select-pane-insertion-up", vec![TargetKind::Client]),
        ("select-pane-insertion-right", vec![TargetKind::Client]),
        ("cycle-pane-placement-span", vec![TargetKind::Client]),
        ("select-next-placement-tab", vec![TargetKind::Client]),
        ("select-previous-placement-tab", vec![TargetKind::Client]),
        ("confirm-pane-placement", vec![TargetKind::Client]),
        ("cancel-pane-placement", vec![TargetKind::Client]),
    ];
    for (action_name, target_kinds) in client_scoped_action_cases {
        let action_metadata = find_action_metadata(action_name);
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

/// Pins which seeds are continuous: the resize-pane, focus-pane, and scroll
/// action families. A new member of a family added without the `continuous`
/// flag — or the flag appearing on any other action — changes this list and
/// fails the assert.
#[test]
fn continuous_action_seeds_are_stable() {
    let mut continuous_action_names: Vec<String> = build_core_action_seeds()
        .iter()
        .filter(|(_, action_metadata)| action_metadata.is_continuous)
        .map(|(action_reference, _)| action_reference.to_string())
        .collect();
    continuous_action_names.sort();

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

    assert_eq!(continuous_action_names, expected_continuous_action_names);
}

/// Pins the exact set of built-in actions. Adding, removing, or renaming a seed
/// changes this list and fails the assert.
#[test]
fn core_action_seed_name_snapshot_is_stable() {
    let mut core_action_names: Vec<String> = build_core_action_seeds()
        .iter()
        .map(|(action_reference, _)| action_reference.to_string())
        .collect();
    core_action_names.sort();

    let expected_core_action_names = vec![
        "core:begin-pane-placement",
        "core:cancel-pane-placement",
        "core:close-pane",
        "core:close-pane-tree",
        "core:close-tab",
        "core:confirm-pane-placement",
        "core:cycle-pane-placement-span",
        "core:focus-pane",
        "core:focus-pane-down",
        "core:focus-pane-left",
        "core:focus-pane-right",
        "core:focus-pane-up",
        "core:focus-tab",
        "core:lock",
        "core:mouse-select",
        "core:move-pane",
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
        "core:select-next-placement-tab",
        "core:select-pane-insertion-down",
        "core:select-pane-insertion-left",
        "core:select-pane-insertion-right",
        "core:select-pane-insertion-up",
        "core:select-pane-target-down",
        "core:select-pane-target-left",
        "core:select-pane-target-right",
        "core:select-pane-target-up",
        "core:select-previous-placement-tab",
        "core:toggle-lock",
        "core:toggle-pane-fullscreen",
        "core:unlock",
        "core:write-to-pane",
    ];
    assert_eq!(core_action_names, expected_core_action_names);
}
