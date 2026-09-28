//! Unit tests for the action registry: the built-in seed load and lookup.

use super::*;

use crate::action::ActionHandlerReference;
use crate::command::CommandKind;

#[test]
fn new_holds_every_seed_with_its_metadata() {
    let registry = ActionRegistry::new();

    for (action_reference, action_metadata) in build_core_action_seeds() {
        assert_eq!(
            registry.find_action_metadata(&action_reference),
            Some(&action_metadata),
            "{action_reference}"
        );
    }
}

#[test]
fn new_lookup_returns_the_seeded_metadata() {
    let registry = ActionRegistry::new();
    let new_pane_action_reference =
        ActionReference::from_core_action_name("new-pane").expect("valid core action name");

    let action_metadata = registry
        .find_action_metadata(&new_pane_action_reference)
        .expect("new-pane is seeded");

    assert_eq!(action_metadata.display_name, "New Pane");
    assert_eq!(
        action_metadata.handler,
        ActionHandlerReference::CoreCommand(CommandKind::NewPane)
    );
}

#[test]
fn lookup_of_an_unseeded_reference_is_none() {
    let registry = ActionRegistry::new();
    let absent_action_reference =
        ActionReference::from_core_action_name("open-status").expect("valid core action name");

    assert_eq!(
        registry.find_action_metadata(&absent_action_reference),
        None
    );
}

#[test]
fn default_registry_matches_new_registry() {
    let default_registry = ActionRegistry::default();

    for (action_reference, action_metadata) in build_core_action_seeds() {
        assert_eq!(
            default_registry.find_action_metadata(&action_reference),
            Some(&action_metadata),
            "{action_reference}"
        );
    }
}
