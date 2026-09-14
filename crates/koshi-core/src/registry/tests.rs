//! Unit tests for the action registry: the built-in seed load, the ownership
//! checks that gate registration and removal, the handler restriction, the
//! per-plugin cap, and the version counter.
//!
//! Also home to [`insert_action_without_validation`], the seam sibling test modules use to place
//! an entry the public surface cannot build.

use super::*;

use crate::action::{
    ActionHandlerReference, ActionMetadata, ActionReference, ActionScope, ActionStatus, TargetKind,
};
use crate::command::CommandKind;
use uuid::Uuid;

/// Insert an entry with none of the ownership checks
/// [`register_action`](ActionRegistry::register_action) makes, and bump the version.
///
/// Accepts any namespace and any handler, including a `user:` reference with
/// a [`Sequence`](ActionHandlerReference::Sequence) handler, which
/// [`register_action`](ActionRegistry::register_action) refuses. An entry already under
/// `action_reference` is replaced.
pub(crate) fn insert_action_without_validation(
    registry: &mut ActionRegistry,
    action_reference: ActionReference,
    action_metadata: ActionMetadata,
) {
    registry
        .action_metadata_by_reference
        .insert(action_reference, action_metadata);
    registry.registry_version += 1;
}

/// A plugin id built from a fixed uuid, so two calls with the same byte yield
/// the same plugin and different bytes yield different plugins.
fn build_test_plugin_id(uuid_fill_byte: u8) -> PluginId {
    PluginId::from_uuid(Uuid::from_bytes([uuid_fill_byte; 16]))
}

/// The metadata [`build_core_action_seeds`] carries for `action_reference`.
///
/// # Panics
/// Panics if `action` is not a seed.
fn get_seeded_action_metadata(action_reference: &ActionReference) -> ActionMetadata {
    build_core_action_seeds()
        .into_iter()
        .find(|(seeded_action_reference, _)| seeded_action_reference == action_reference)
        .map(|(_, action_metadata)| action_metadata)
        .unwrap_or_else(|| panic!("{action_reference} is seeded"))
}

/// Metadata a plugin's own registration carries: its namespace, and a handler
/// routing back to itself.
fn build_plugin_action_metadata(plugin_id: PluginId) -> ActionMetadata {
    ActionMetadata {
        namespace: ActionNamespace::Plugin(plugin_id),
        display_name: "Open Status".to_string(),
        description: "Open the plugin's status panel".to_string(),
        scope: ActionScope::Global,
        target_kinds: vec![TargetKind::Session],
        handler: ActionHandlerReference::PluginHostCall(plugin_id),
        action_status: ActionStatus::Available,
        is_continuous: false,
    }
}

#[test]
fn new_seeds_every_core_action_at_version_zero() {
    let registry = ActionRegistry::new();

    assert_eq!(registry.get_registry_version(), 0);
    assert_eq!(
        registry
            .list_actions_by_namespace(ActionNamespace::Core)
            .count(),
        build_core_action_seeds().len()
    );
}

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
fn listing_user_and_unknown_plugin_actions_is_empty_on_new_registry() {
    let registry = ActionRegistry::new();

    assert_eq!(
        registry
            .list_actions_by_namespace(ActionNamespace::User)
            .count(),
        0
    );
    assert_eq!(
        registry
            .list_actions_by_namespace(ActionNamespace::Plugin(build_test_plugin_id(1)))
            .count(),
        0
    );
}

#[test]
fn new_lookup_returns_the_seeded_metadata() {
    let registry = ActionRegistry::new();
    let new_pane_action_reference =
        ActionReference::from_core_action_name("new-pane").expect("valid core action name");

    let action_metadata = registry
        .find_action_metadata(&new_pane_action_reference)
        .expect("new-pane is seeded");

    assert_eq!(action_metadata.namespace, ActionNamespace::Core);
    assert_eq!(action_metadata.display_name, "New Pane");
    assert_eq!(
        action_metadata.handler,
        ActionHandlerReference::CoreCommand(CommandKind::NewPane)
    );
    assert_eq!(action_metadata.action_status, ActionStatus::Available);
}

#[test]
fn lookup_of_an_unregistered_reference_is_none() {
    let registry = ActionRegistry::new();
    let absent_action_reference =
        ActionReference::from_plugin_action_name(build_test_plugin_id(1), "open-status")
            .expect("valid plugin action name");

    assert_eq!(
        registry.find_action_metadata(&absent_action_reference),
        None
    );
}

#[test]
fn register_adds_a_plugin_action_and_bumps_registry_version() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "open-status")
        .expect("valid plugin action name");
    let action_metadata = build_plugin_action_metadata(plugin_id);

    assert_eq!(
        registry.register_action(plugin_id, action_reference.clone(), action_metadata.clone()),
        Ok(())
    );

    assert_eq!(
        registry.find_action_metadata(&action_reference),
        Some(&action_metadata)
    );
    assert_eq!(registry.get_registry_version(), 1);
}

#[test]
fn register_rejects_the_core_namespace() {
    let mut registry = ActionRegistry::new();
    let caller_plugin_id = build_test_plugin_id(1);
    let action_reference =
        ActionReference::from_core_action_name("take-over").expect("valid core action name");

    assert_eq!(
        registry.register_action(
            caller_plugin_id,
            action_reference.clone(),
            build_plugin_action_metadata(caller_plugin_id),
        ),
        Err(RegistryError::ReservedNamespace { action_reference })
    );
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_rejects_the_user_namespace() {
    let mut registry = ActionRegistry::new();
    let caller_plugin_id = build_test_plugin_id(1);
    let action_reference =
        ActionReference::from_user_action_name("my-macro").expect("valid user action name");

    assert_eq!(
        registry.register_action(
            caller_plugin_id,
            action_reference.clone(),
            build_plugin_action_metadata(caller_plugin_id),
        ),
        Err(RegistryError::ReservedNamespace { action_reference })
    );
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_rejects_a_caller_squatting_another_plugins_namespace() {
    let mut registry = ActionRegistry::new();
    let caller_plugin_id = build_test_plugin_id(1);
    let victim_plugin_id = build_test_plugin_id(2);
    let action_reference =
        ActionReference::from_plugin_action_name(victim_plugin_id, "open-status")
            .expect("valid plugin action name");

    assert_eq!(
        registry.register_action(
            caller_plugin_id,
            action_reference.clone(),
            build_plugin_action_metadata(victim_plugin_id),
        ),
        Err(RegistryError::ForeignNamespace {
            action_reference: action_reference.clone(),
            caller_plugin_id,
        })
    );
    assert_eq!(registry.find_action_metadata(&action_reference), None);
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_rejects_metadata_whose_namespace_disagrees_with_the_ref() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "open-status")
        .expect("valid plugin action name");
    let mut action_metadata = build_plugin_action_metadata(plugin_id);
    action_metadata.namespace = ActionNamespace::Core;

    assert_eq!(
        registry.register_action(plugin_id, action_reference.clone(), action_metadata),
        Err(RegistryError::NamespaceMismatch {
            action_reference: action_reference.clone()
        })
    );
    assert_eq!(registry.find_action_metadata(&action_reference), None);
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_rejects_metadata_claiming_another_plugin_owns_the_ref() {
    let mut registry = ActionRegistry::new();
    let owner_plugin_id = build_test_plugin_id(1);
    let other_plugin_id = build_test_plugin_id(2);
    let action_reference = ActionReference::from_plugin_action_name(owner_plugin_id, "open-status")
        .expect("valid plugin action name");
    let mut action_metadata = build_plugin_action_metadata(owner_plugin_id);
    action_metadata.namespace = ActionNamespace::Plugin(other_plugin_id);

    assert_eq!(
        registry.register_action(owner_plugin_id, action_reference.clone(), action_metadata),
        Err(RegistryError::NamespaceMismatch {
            action_reference: action_reference.clone()
        })
    );
    assert_eq!(registry.find_action_metadata(&action_reference), None);
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_rejects_a_handler_routing_to_another_plugin() {
    let mut registry = ActionRegistry::new();
    let owner_plugin_id = build_test_plugin_id(1);
    let other_plugin_id = build_test_plugin_id(2);
    let action_reference = ActionReference::from_plugin_action_name(owner_plugin_id, "open-status")
        .expect("valid plugin action name");
    let mut action_metadata = build_plugin_action_metadata(owner_plugin_id);
    action_metadata.handler = ActionHandlerReference::PluginHostCall(other_plugin_id);

    assert_eq!(
        registry.register_action(owner_plugin_id, action_reference.clone(), action_metadata),
        Err(RegistryError::InvalidHandler {
            action_reference: action_reference.clone()
        })
    );
    assert_eq!(registry.find_action_metadata(&action_reference), None);
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_rejects_a_core_command_handler_that_would_skip_the_capability_check() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "inject-keys")
        .expect("valid plugin action name");
    let mut action_metadata = build_plugin_action_metadata(plugin_id);
    action_metadata.handler = ActionHandlerReference::CoreCommand(CommandKind::WriteToPane);

    assert_eq!(
        registry.register_action(plugin_id, action_reference.clone(), action_metadata),
        Err(RegistryError::InvalidHandler {
            action_reference: action_reference.clone()
        })
    );
    assert_eq!(registry.find_action_metadata(&action_reference), None);
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_rejects_a_sequence_handler_naming_another_plugin() {
    let mut registry = ActionRegistry::new();
    let owner_plugin_id = build_test_plugin_id(1);
    let other_plugin_id = build_test_plugin_id(2);
    let action_reference = ActionReference::from_plugin_action_name(owner_plugin_id, "chain")
        .expect("valid plugin action name");
    let foreign_action_reference =
        ActionReference::from_plugin_action_name(other_plugin_id, "open-status")
            .expect("valid plugin action name");
    let mut action_metadata = build_plugin_action_metadata(owner_plugin_id);
    action_metadata.handler = ActionHandlerReference::Sequence(vec![foreign_action_reference]);

    assert_eq!(
        registry.register_action(owner_plugin_id, action_reference.clone(), action_metadata),
        Err(RegistryError::InvalidHandler {
            action_reference: action_reference.clone()
        })
    );
    assert_eq!(registry.find_action_metadata(&action_reference), None);
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_rejects_a_sequence_handler_naming_only_core_actions() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "chain")
        .expect("valid plugin action name");
    let new_pane_action_reference =
        ActionReference::from_core_action_name("new-pane").expect("valid core action name");
    let mut action_metadata = build_plugin_action_metadata(plugin_id);
    action_metadata.handler = ActionHandlerReference::Sequence(vec![new_pane_action_reference]);

    assert_eq!(
        registry.register_action(plugin_id, action_reference.clone(), action_metadata),
        Err(RegistryError::InvalidHandler {
            action_reference: action_reference.clone()
        })
    );
    assert_eq!(registry.find_action_metadata(&action_reference), None);
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_rejects_a_sequence_handler_naming_the_callers_own_actions() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "chain")
        .expect("valid plugin action name");
    let own_action_reference = ActionReference::from_plugin_action_name(plugin_id, "open-status")
        .expect("valid plugin action name");
    let mut action_metadata = build_plugin_action_metadata(plugin_id);
    action_metadata.handler = ActionHandlerReference::Sequence(vec![own_action_reference]);

    assert_eq!(
        registry.register_action(plugin_id, action_reference.clone(), action_metadata),
        Err(RegistryError::InvalidHandler {
            action_reference: action_reference.clone()
        })
    );
    assert_eq!(registry.find_action_metadata(&action_reference), None);
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_prioritizes_foreign_namespace_over_a_disagreeing_metadata_namespace() {
    // Two rejection reasons are true at once here: `caller_plugin_id` does not own
    // `action_reference`'s namespace (`ForeignNamespace`, step 1 of
    // `register_action`), *and* `action_metadata.namespace` disagrees with
    // `action_reference.namespace`
    // (`NamespaceMismatch`, step 2). The check order in `register_action` means
    // `ForeignNamespace` wins.
    let mut registry = ActionRegistry::new();
    let caller_plugin_id = build_test_plugin_id(1);
    let victim_plugin_id = build_test_plugin_id(2);
    let action_reference =
        ActionReference::from_plugin_action_name(victim_plugin_id, "open-status")
            .expect("valid plugin action name");
    let mut action_metadata = build_plugin_action_metadata(victim_plugin_id);
    action_metadata.namespace = ActionNamespace::Core;

    assert_eq!(
        registry.register_action(caller_plugin_id, action_reference.clone(), action_metadata),
        Err(RegistryError::ForeignNamespace {
            action_reference,
            caller_plugin_id,
        })
    );
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_prioritizes_namespace_mismatch_over_an_invalid_handler() {
    // `action_metadata.namespace` disagrees with `action_reference.namespace` (step 2) *and*
    // the handler routes to another plugin (step 3). Step 2 wins.
    let mut registry = ActionRegistry::new();
    let owner_plugin_id = build_test_plugin_id(1);
    let other_plugin_id = build_test_plugin_id(2);
    let action_reference = ActionReference::from_plugin_action_name(owner_plugin_id, "open-status")
        .expect("valid plugin action name");
    let mut action_metadata = build_plugin_action_metadata(owner_plugin_id);
    action_metadata.namespace = ActionNamespace::Core;
    action_metadata.handler = ActionHandlerReference::PluginHostCall(other_plugin_id);

    assert_eq!(
        registry.register_action(owner_plugin_id, action_reference.clone(), action_metadata),
        Err(RegistryError::NamespaceMismatch { action_reference })
    );
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn register_prioritizes_an_invalid_handler_over_a_duplicate() {
    // The reference is already registered (step 4) *and* the new metadata's
    // handler is a core command (step 3). Step 3 wins.
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "open-status")
        .expect("valid plugin action name");
    registry
        .register_action(
            plugin_id,
            action_reference.clone(),
            build_plugin_action_metadata(plugin_id),
        )
        .expect("first registration succeeds");
    let mut action_metadata = build_plugin_action_metadata(plugin_id);
    action_metadata.handler = ActionHandlerReference::CoreCommand(CommandKind::Quit);

    assert_eq!(
        registry.register_action(plugin_id, action_reference.clone(), action_metadata),
        Err(RegistryError::InvalidHandler { action_reference })
    );
    assert_eq!(registry.get_registry_version(), 1);
}

#[test]
fn register_prioritizes_a_duplicate_over_the_cap() {
    // The plugin holds the maximum (step 5) *and* re-registers one of its own
    // references (step 4). Step 4 wins.
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    for plugin_action_index in 0..MAX_PLUGIN_ACTION_COUNT {
        let action_reference = ActionReference::from_plugin_action_name(
            plugin_id,
            &format!("action-{plugin_action_index}"),
        )
        .expect("valid plugin action name");
        registry
            .register_action(
                plugin_id,
                action_reference,
                build_plugin_action_metadata(plugin_id),
            )
            .expect("registration below the cap succeeds");
    }
    let registered_action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "action-0")
            .expect("valid plugin action name");

    assert_eq!(
        registry.register_action(
            plugin_id,
            registered_action_reference.clone(),
            build_plugin_action_metadata(plugin_id),
        ),
        Err(RegistryError::Duplicate {
            action_reference: registered_action_reference
        })
    );
    assert_eq!(
        registry.get_registry_version(),
        MAX_PLUGIN_ACTION_COUNT as u64
    );
}

#[test]
fn a_duplicate_registration_leaves_the_first_entry_untouched() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "open-status")
        .expect("valid plugin action name");
    let existing_action_metadata = build_plugin_action_metadata(plugin_id);
    registry
        .register_action(
            plugin_id,
            action_reference.clone(),
            existing_action_metadata.clone(),
        )
        .expect("first registration succeeds");
    let mut replacement_action_metadata = build_plugin_action_metadata(plugin_id);
    replacement_action_metadata.display_name = "Hijacked".to_string();

    assert_eq!(
        registry.register_action(
            plugin_id,
            action_reference.clone(),
            replacement_action_metadata
        ),
        Err(RegistryError::Duplicate {
            action_reference: action_reference.clone()
        })
    );
    assert_eq!(
        registry.find_action_metadata(&action_reference),
        Some(&existing_action_metadata)
    );
}

#[test]
fn register_rejects_a_duplicate_ref_without_bumping_registry_version() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "open-status")
        .expect("valid plugin action name");
    registry
        .register_action(
            plugin_id,
            action_reference.clone(),
            build_plugin_action_metadata(plugin_id),
        )
        .expect("first registration succeeds");

    assert_eq!(
        registry.register_action(
            plugin_id,
            action_reference.clone(),
            build_plugin_action_metadata(plugin_id),
        ),
        Err(RegistryError::Duplicate { action_reference })
    );
    assert_eq!(registry.get_registry_version(), 1);
}

#[test]
fn register_rejects_the_thirty_third_action_of_one_plugin() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    for plugin_action_index in 0..MAX_PLUGIN_ACTION_COUNT {
        let action_reference = ActionReference::from_plugin_action_name(
            plugin_id,
            &format!("action-{plugin_action_index}"),
        )
        .expect("valid plugin action name");
        registry
            .register_action(
                plugin_id,
                action_reference,
                build_plugin_action_metadata(plugin_id),
            )
            .expect("registration below the cap succeeds");
    }

    let over_limit_action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "one-too-many")
            .expect("valid plugin action name");
    assert_eq!(
        registry.register_action(
            plugin_id,
            over_limit_action_reference,
            build_plugin_action_metadata(plugin_id),
        ),
        Err(RegistryError::PluginActionLimitExceeded {
            caller_plugin_id: plugin_id,
            maximum_action_count: MAX_PLUGIN_ACTION_COUNT,
        })
    );
    assert_eq!(
        registry.get_registry_version(),
        MAX_PLUGIN_ACTION_COUNT as u64
    );
}

#[test]
fn the_cap_is_counted_per_plugin_not_across_plugins() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    for plugin_action_index in 0..MAX_PLUGIN_ACTION_COUNT {
        let action_reference = ActionReference::from_plugin_action_name(
            plugin_id,
            &format!("action-{plugin_action_index}"),
        )
        .expect("valid plugin action name");
        registry
            .register_action(
                plugin_id,
                action_reference,
                build_plugin_action_metadata(plugin_id),
            )
            .expect("registration below the cap succeeds");
    }

    let other_plugin_id = build_test_plugin_id(2);
    let action_reference = ActionReference::from_plugin_action_name(other_plugin_id, "open-status")
        .expect("valid plugin action name");
    assert_eq!(
        registry.register_action(
            other_plugin_id,
            action_reference,
            build_plugin_action_metadata(other_plugin_id),
        ),
        Ok(())
    );
    assert_eq!(
        registry.get_registry_version(),
        MAX_PLUGIN_ACTION_COUNT as u64 + 1
    );
}

#[test]
fn remove_action_removes_plugin_action_and_bumps_registry_version() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "open-status")
        .expect("valid plugin action name");
    let action_metadata = build_plugin_action_metadata(plugin_id);
    registry
        .register_action(plugin_id, action_reference.clone(), action_metadata.clone())
        .expect("registration succeeds");

    assert_eq!(
        registry.remove_action(plugin_id, &action_reference),
        Some(action_metadata)
    );

    assert_eq!(registry.find_action_metadata(&action_reference), None);
    assert_eq!(registry.get_registry_version(), 2);
}

#[test]
fn remove_action_of_absent_reference_returns_none_and_preserves_version() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let absent_action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "open-status")
            .expect("valid plugin action name");

    assert_eq!(
        registry.remove_action(plugin_id, &absent_action_reference),
        None
    );
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn remove_action_never_removes_another_plugins_action() {
    let mut registry = ActionRegistry::new();
    let owner_plugin_id = build_test_plugin_id(1);
    let attacker_plugin_id = build_test_plugin_id(2);
    let action_reference = ActionReference::from_plugin_action_name(owner_plugin_id, "open-status")
        .expect("valid plugin action name");
    registry
        .register_action(
            owner_plugin_id,
            action_reference.clone(),
            build_plugin_action_metadata(owner_plugin_id),
        )
        .expect("registration succeeds");

    assert_eq!(
        registry.remove_action(attacker_plugin_id, &action_reference),
        None
    );

    assert_eq!(
        registry.find_action_metadata(&action_reference),
        Some(&build_plugin_action_metadata(owner_plugin_id))
    );
    assert_eq!(registry.get_registry_version(), 1);
}

#[test]
fn remove_action_never_removes_a_core_action() {
    let mut registry = ActionRegistry::new();
    let new_pane_action_reference =
        ActionReference::from_core_action_name("new-pane").expect("valid core action name");
    let seeded_action_metadata = get_seeded_action_metadata(&new_pane_action_reference);

    assert_eq!(
        registry.remove_action(build_test_plugin_id(1), &new_pane_action_reference),
        None
    );

    assert_eq!(
        registry.find_action_metadata(&new_pane_action_reference),
        Some(&seeded_action_metadata)
    );
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn second_remove_action_of_same_reference_returns_none_and_preserves_version() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "open-status")
        .expect("valid plugin action name");
    registry
        .register_action(
            plugin_id,
            action_reference.clone(),
            build_plugin_action_metadata(plugin_id),
        )
        .expect("registration succeeds");
    registry
        .remove_action(plugin_id, &action_reference)
        .expect("first remove_action removes the entry");

    assert_eq!(registry.remove_action(plugin_id, &action_reference), None);
    assert_eq!(registry.get_registry_version(), 2);
}

#[test]
fn action_reference_can_be_registered_again_after_removal() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "open-status")
        .expect("valid plugin action name");
    registry
        .register_action(
            plugin_id,
            action_reference.clone(),
            build_plugin_action_metadata(plugin_id),
        )
        .expect("registration succeeds");
    registry
        .remove_action(plugin_id, &action_reference)
        .expect("remove_action removes the entry");
    let mut replacement_action_metadata = build_plugin_action_metadata(plugin_id);
    replacement_action_metadata.display_name = "Open Status Again".to_string();

    assert_eq!(
        registry.register_action(
            plugin_id,
            action_reference.clone(),
            replacement_action_metadata.clone(),
        ),
        Ok(())
    );
    assert_eq!(
        registry.find_action_metadata(&action_reference),
        Some(&replacement_action_metadata)
    );
    assert_eq!(registry.get_registry_version(), 3);
}

#[test]
fn remove_action_frees_a_slot_under_the_cap() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    for plugin_action_index in 0..MAX_PLUGIN_ACTION_COUNT {
        let action_reference = ActionReference::from_plugin_action_name(
            plugin_id,
            &format!("action-{plugin_action_index}"),
        )
        .expect("valid plugin action name");
        registry
            .register_action(
                plugin_id,
                action_reference,
                build_plugin_action_metadata(plugin_id),
            )
            .expect("registration below the cap succeeds");
    }
    let freed_action_reference = ActionReference::from_plugin_action_name(plugin_id, "action-0")
        .expect("valid plugin action name");
    registry
        .remove_action(plugin_id, &freed_action_reference)
        .expect("remove_action removes the entry");

    let replacement_action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "replacement")
            .expect("valid plugin action name");
    assert_eq!(
        registry.register_action(
            plugin_id,
            replacement_action_reference.clone(),
            build_plugin_action_metadata(plugin_id),
        ),
        Ok(())
    );
    assert_eq!(
        registry.find_action_metadata(&replacement_action_reference),
        Some(&build_plugin_action_metadata(plugin_id))
    );
    assert_eq!(
        registry.get_registry_version(),
        MAX_PLUGIN_ACTION_COUNT as u64 + 2
    );
}

#[test]
fn remove_action_never_removes_a_user_action() {
    let mut registry = ActionRegistry::new();
    let macro_action_reference =
        ActionReference::from_user_action_name("my-macro").expect("valid user action name");

    assert_eq!(
        registry.remove_action(build_test_plugin_id(1), &macro_action_reference),
        None
    );
    assert_eq!(registry.get_registry_version(), 0);
}

#[test]
fn list_actions_by_namespace_returns_only_one_plugin_namespace() {
    let mut registry = ActionRegistry::new();
    let first_plugin_id = build_test_plugin_id(1);
    let second_plugin_id = build_test_plugin_id(2);
    let first_action_reference =
        ActionReference::from_plugin_action_name(first_plugin_id, "open-status")
            .expect("valid plugin action name");
    let second_action_reference =
        ActionReference::from_plugin_action_name(second_plugin_id, "open-status")
            .expect("valid plugin action name");
    registry
        .register_action(
            first_plugin_id,
            first_action_reference.clone(),
            build_plugin_action_metadata(first_plugin_id),
        )
        .expect("registration succeeds");
    registry
        .register_action(
            second_plugin_id,
            second_action_reference,
            build_plugin_action_metadata(second_plugin_id),
        )
        .expect("registration succeeds");

    let listed_action_references: Vec<&ActionReference> = registry
        .list_actions_by_namespace(ActionNamespace::Plugin(first_plugin_id))
        .map(|(action_reference, _)| action_reference)
        .collect();

    assert_eq!(listed_action_references, vec![&first_action_reference]);
}

#[test]
fn a_plugin_registration_leaves_the_core_namespace_untouched() {
    let mut registry = ActionRegistry::new();
    let plugin_id = build_test_plugin_id(1);
    let action_reference = ActionReference::from_plugin_action_name(plugin_id, "open-status")
        .expect("valid plugin action name");
    registry
        .register_action(
            plugin_id,
            action_reference,
            build_plugin_action_metadata(plugin_id),
        )
        .expect("registration succeeds");

    assert_eq!(
        registry
            .list_actions_by_namespace(ActionNamespace::Core)
            .count(),
        build_core_action_seeds().len()
    );
}

#[test]
fn default_registry_matches_new_registry() {
    let default_registry = ActionRegistry::default();
    let new_registry = ActionRegistry::new();

    assert_eq!(
        default_registry.get_registry_version(),
        new_registry.get_registry_version()
    );
    assert_eq!(
        default_registry
            .list_actions_by_namespace(ActionNamespace::Core)
            .count(),
        new_registry
            .list_actions_by_namespace(ActionNamespace::Core)
            .count()
    );
}

#[test]
fn registry_error_messages_name_the_offender() {
    let action_reference =
        ActionReference::from_core_action_name("new-pane").expect("valid core action name");
    let plugin_id = build_test_plugin_id(1);

    assert_eq!(
        RegistryError::Duplicate {
            action_reference: action_reference.clone()
        }
        .to_string(),
        "action core:new-pane is already registered"
    );
    assert_eq!(
        RegistryError::ReservedNamespace { action_reference }.to_string(),
        "action core:new-pane is in a reserved namespace; only plugin: actions may be registered"
    );
    assert_eq!(
        RegistryError::PluginActionLimitExceeded {
            caller_plugin_id: plugin_id,
            maximum_action_count: MAX_PLUGIN_ACTION_COUNT,
        }
        .to_string(),
        format!(
            "plugin-01010101-0101-0101-0101-010101010101 already holds the maximum of {MAX_PLUGIN_ACTION_COUNT} actions"
        )
    );
}

#[test]
fn registry_error_ownership_messages_name_the_offender() {
    let action_reference =
        ActionReference::from_plugin_action_name(build_test_plugin_id(1), "open-status")
            .expect("valid plugin action name");

    assert_eq!(
        RegistryError::ForeignNamespace {
            action_reference: action_reference.clone(),
            caller_plugin_id: build_test_plugin_id(2),
        }
        .to_string(),
        "action plugin:01010101-0101-0101-0101-010101010101:open-status is not owned by \
         plugin-02020202-0202-0202-0202-020202020202, which may only register in its own namespace"
    );
    assert_eq!(
        RegistryError::NamespaceMismatch {
            action_reference: action_reference.clone()
        }
        .to_string(),
        "action plugin:01010101-0101-0101-0101-010101010101:open-status \
         carries metadata for a different namespace"
    );
    assert_eq!(
        RegistryError::InvalidHandler { action_reference }.to_string(),
        "action plugin:01010101-0101-0101-0101-010101010101:open-status \
         must dispatch through its owning plugin's host call"
    );
}

#[test]
fn registry_error_is_a_recoverable_plugin_failure() {
    let registry_error = RegistryError::Duplicate {
        action_reference: ActionReference::from_plugin_action_name(
            build_test_plugin_id(1),
            "open-status",
        )
        .expect("valid plugin action name"),
    };

    assert_eq!(registry_error.category(), DomainCategory::Plugin);
    assert_eq!(registry_error.get_severity(), Severity::Recoverable);
}

#[test]
fn register_strips_control_and_bidi_characters_from_the_plugin_text() {
    let plugin_id = build_test_plugin_id(9);
    let action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "status").expect("valid");
    let mut action_metadata = build_plugin_action_metadata(plugin_id);
    action_metadata.display_name = "Open\u{7f} Status".to_string();
    action_metadata.description = "\u{202e}gpj.exe".to_string();
    let mut registry = ActionRegistry::new();

    registry
        .register_action(plugin_id, action_reference.clone(), action_metadata)
        .expect("registered");

    let registered_action_metadata = registry
        .find_action_metadata(&action_reference)
        .expect("registered");
    assert_eq!(registered_action_metadata.display_name, "Open Status");
    assert_eq!(registered_action_metadata.description, "gpj.exe");
}

#[test]
fn register_cuts_the_plugin_text_to_the_reported_text_cap() {
    let plugin_id = build_test_plugin_id(10);
    let action_reference =
        ActionReference::from_plugin_action_name(plugin_id, "status").expect("valid");
    let mut action_metadata = build_plugin_action_metadata(plugin_id);
    action_metadata.display_name = "a".repeat(crate::text::MAX_REPORTED_TEXT_BYTE_COUNT + 100);
    action_metadata.description = "b".repeat(crate::text::MAX_REPORTED_TEXT_BYTE_COUNT + 1);
    let mut registry = ActionRegistry::new();

    registry
        .register_action(plugin_id, action_reference.clone(), action_metadata)
        .expect("registered");

    let registered_action_metadata = registry
        .find_action_metadata(&action_reference)
        .expect("registered");
    assert_eq!(
        registered_action_metadata.display_name,
        "a".repeat(crate::text::MAX_REPORTED_TEXT_BYTE_COUNT)
    );
    assert_eq!(
        registered_action_metadata.description,
        "b".repeat(crate::text::MAX_REPORTED_TEXT_BYTE_COUNT)
    );
}
