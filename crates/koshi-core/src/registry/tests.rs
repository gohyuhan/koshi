//! Unit tests for the action registry: the built-in seed load, the ownership
//! checks that gate registration and removal, the handler restriction, the
//! per-plugin cap, and the version counter.
//!
//! Also home to [`insert_unchecked`], the seam sibling test modules use to place
//! an entry the public surface cannot build.

use super::*;

use crate::action::{
    ActionHandlerRef, ActionMetadata, ActionRef, ActionScope, ActionStatus, TargetKind,
};
use crate::command::CommandKind;
use uuid::Uuid;

/// Insert an entry with none of the ownership checks
/// [`register`](ActionRegistry::register) makes, and bump the version.
///
/// Accepts any namespace and any handler, including a `user:` reference with
/// a [`Sequence`](ActionHandlerRef::Sequence) handler, which
/// [`register`](ActionRegistry::register) refuses. An entry already under
/// `action` is replaced.
pub(crate) fn insert_unchecked(
    registry: &mut ActionRegistry,
    action: ActionRef,
    metadata: ActionMetadata,
) {
    registry.entries.insert(action, metadata);
    registry.version += 1;
}

/// A plugin id built from a fixed uuid, so two calls with the same byte yield
/// the same plugin and different bytes yield different plugins.
fn plugin_id(byte: u8) -> PluginId {
    PluginId::from_uuid(Uuid::from_bytes([byte; 16]))
}

/// The metadata [`core_action_seeds`] carries for `action`.
///
/// # Panics
/// Panics if `action` is not a seed.
fn seeded_metadata(action: &ActionRef) -> ActionMetadata {
    core_action_seeds()
        .into_iter()
        .find(|(seed, _)| seed == action)
        .map(|(_, metadata)| metadata)
        .unwrap_or_else(|| panic!("{action} is seeded"))
}

/// Metadata a plugin's own registration carries: its namespace, and a handler
/// routing back to itself.
fn plugin_metadata(plugin: PluginId) -> ActionMetadata {
    ActionMetadata {
        namespace: ActionNamespace::Plugin(plugin),
        display_name: "Open Status".to_string(),
        description: "Open the plugin's status panel".to_string(),
        scope_class: ActionScope::Global,
        target_compat: vec![TargetKind::Session],
        handler: ActionHandlerRef::PluginHostCall(plugin),
        status: ActionStatus::Available,
        continuous: false,
    }
}

#[test]
fn new_seeds_every_core_action_at_version_zero() {
    let registry = ActionRegistry::new();

    assert_eq!(registry.version(), 0);
    assert_eq!(
        registry.list_by_namespace(ActionNamespace::Core).count(),
        core_action_seeds().len()
    );
}

#[test]
fn new_holds_every_seed_with_its_metadata() {
    let registry = ActionRegistry::new();

    for (action, metadata) in core_action_seeds() {
        assert_eq!(registry.lookup(&action), Some(&metadata), "{action}");
    }
}

#[test]
fn list_by_namespace_of_user_and_of_an_unknown_plugin_is_empty_on_a_new_registry() {
    let registry = ActionRegistry::new();

    assert_eq!(registry.list_by_namespace(ActionNamespace::User).count(), 0);
    assert_eq!(
        registry
            .list_by_namespace(ActionNamespace::Plugin(plugin_id(1)))
            .count(),
        0
    );
}

#[test]
fn new_lookup_returns_the_seeded_metadata() {
    let registry = ActionRegistry::new();
    let new_pane = ActionRef::core("new-pane").expect("valid core action name");

    let metadata = registry.lookup(&new_pane).expect("new-pane is seeded");

    assert_eq!(metadata.namespace, ActionNamespace::Core);
    assert_eq!(metadata.display_name, "New Pane");
    assert_eq!(
        metadata.handler,
        ActionHandlerRef::CoreCommand(CommandKind::NewPane)
    );
    assert_eq!(metadata.status, ActionStatus::Available);
}

#[test]
fn lookup_of_an_unregistered_ref_is_none() {
    let registry = ActionRegistry::new();
    let absent = ActionRef::plugin(plugin_id(1), "open-status").expect("valid plugin action name");

    assert_eq!(registry.lookup(&absent), None);
}

#[test]
fn register_adds_a_plugin_action_and_bumps_the_version() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    let metadata = plugin_metadata(plugin);

    assert_eq!(
        registry.register(plugin, action.clone(), metadata.clone()),
        Ok(())
    );

    assert_eq!(registry.lookup(&action), Some(&metadata));
    assert_eq!(registry.version(), 1);
}

#[test]
fn register_rejects_the_core_namespace() {
    let mut registry = ActionRegistry::new();
    let caller = plugin_id(1);
    let action = ActionRef::core("take-over").expect("valid core action name");

    assert_eq!(
        registry.register(caller, action.clone(), plugin_metadata(caller)),
        Err(RegistryError::ReservedNamespace { action })
    );
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_rejects_the_user_namespace() {
    let mut registry = ActionRegistry::new();
    let caller = plugin_id(1);
    let action = ActionRef::user("my-macro").expect("valid user action name");

    assert_eq!(
        registry.register(caller, action.clone(), plugin_metadata(caller)),
        Err(RegistryError::ReservedNamespace { action })
    );
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_rejects_a_caller_squatting_another_plugins_namespace() {
    let mut registry = ActionRegistry::new();
    let caller = plugin_id(1);
    let victim = plugin_id(2);
    let action = ActionRef::plugin(victim, "open-status").expect("valid plugin action name");

    assert_eq!(
        registry.register(caller, action.clone(), plugin_metadata(victim)),
        Err(RegistryError::ForeignNamespace {
            action: action.clone(),
            caller,
        })
    );
    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_rejects_metadata_whose_namespace_disagrees_with_the_ref() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    let mut metadata = plugin_metadata(plugin);
    metadata.namespace = ActionNamespace::Core;

    assert_eq!(
        registry.register(plugin, action.clone(), metadata),
        Err(RegistryError::NamespaceMismatch {
            action: action.clone()
        })
    );
    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_rejects_metadata_claiming_another_plugin_owns_the_ref() {
    let mut registry = ActionRegistry::new();
    let owner = plugin_id(1);
    let other = plugin_id(2);
    let action = ActionRef::plugin(owner, "open-status").expect("valid plugin action name");
    let mut metadata = plugin_metadata(owner);
    metadata.namespace = ActionNamespace::Plugin(other);

    assert_eq!(
        registry.register(owner, action.clone(), metadata),
        Err(RegistryError::NamespaceMismatch {
            action: action.clone()
        })
    );
    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_rejects_a_handler_routing_to_another_plugin() {
    let mut registry = ActionRegistry::new();
    let owner = plugin_id(1);
    let other = plugin_id(2);
    let action = ActionRef::plugin(owner, "open-status").expect("valid plugin action name");
    let mut metadata = plugin_metadata(owner);
    metadata.handler = ActionHandlerRef::PluginHostCall(other);

    assert_eq!(
        registry.register(owner, action.clone(), metadata),
        Err(RegistryError::InvalidHandler {
            action: action.clone()
        })
    );
    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_rejects_a_core_command_handler_that_would_skip_the_capability_check() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "inject-keys").expect("valid plugin action name");
    let mut metadata = plugin_metadata(plugin);
    metadata.handler = ActionHandlerRef::CoreCommand(CommandKind::WriteToPane);

    assert_eq!(
        registry.register(plugin, action.clone(), metadata),
        Err(RegistryError::InvalidHandler {
            action: action.clone()
        })
    );
    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_rejects_a_sequence_handler_naming_another_plugin() {
    let mut registry = ActionRegistry::new();
    let owner = plugin_id(1);
    let other = plugin_id(2);
    let action = ActionRef::plugin(owner, "chain").expect("valid plugin action name");
    let foreign = ActionRef::plugin(other, "open-status").expect("valid plugin action name");
    let mut metadata = plugin_metadata(owner);
    metadata.handler = ActionHandlerRef::Sequence(vec![foreign]);

    assert_eq!(
        registry.register(owner, action.clone(), metadata),
        Err(RegistryError::InvalidHandler {
            action: action.clone()
        })
    );
    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_rejects_a_sequence_handler_naming_only_core_actions() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "chain").expect("valid plugin action name");
    let new_pane = ActionRef::core("new-pane").expect("valid core action name");
    let mut metadata = plugin_metadata(plugin);
    metadata.handler = ActionHandlerRef::Sequence(vec![new_pane]);

    assert_eq!(
        registry.register(plugin, action.clone(), metadata),
        Err(RegistryError::InvalidHandler {
            action: action.clone()
        })
    );
    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_rejects_a_sequence_handler_naming_the_callers_own_actions() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "chain").expect("valid plugin action name");
    let own = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    let mut metadata = plugin_metadata(plugin);
    metadata.handler = ActionHandlerRef::Sequence(vec![own]);

    assert_eq!(
        registry.register(plugin, action.clone(), metadata),
        Err(RegistryError::InvalidHandler {
            action: action.clone()
        })
    );
    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_prioritizes_foreign_namespace_over_a_disagreeing_metadata_namespace() {
    // Two rejection reasons are true at once here: `caller` does not own
    // `action`'s namespace (`ForeignNamespace`, step 1 of `register`), *and*
    // `metadata.namespace` disagrees with `action.namespace`
    // (`NamespaceMismatch`, step 2). The check order in `register` means
    // `ForeignNamespace` wins.
    let mut registry = ActionRegistry::new();
    let caller = plugin_id(1);
    let victim = plugin_id(2);
    let action = ActionRef::plugin(victim, "open-status").expect("valid plugin action name");
    let mut metadata = plugin_metadata(victim);
    metadata.namespace = ActionNamespace::Core;

    assert_eq!(
        registry.register(caller, action.clone(), metadata),
        Err(RegistryError::ForeignNamespace { action, caller })
    );
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_prioritizes_namespace_mismatch_over_an_invalid_handler() {
    // `metadata.namespace` disagrees with `action.namespace` (step 2) *and*
    // the handler routes to another plugin (step 3). Step 2 wins.
    let mut registry = ActionRegistry::new();
    let owner = plugin_id(1);
    let other = plugin_id(2);
    let action = ActionRef::plugin(owner, "open-status").expect("valid plugin action name");
    let mut metadata = plugin_metadata(owner);
    metadata.namespace = ActionNamespace::Core;
    metadata.handler = ActionHandlerRef::PluginHostCall(other);

    assert_eq!(
        registry.register(owner, action.clone(), metadata),
        Err(RegistryError::NamespaceMismatch { action })
    );
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_prioritizes_an_invalid_handler_over_a_duplicate() {
    // The reference is already registered (step 4) *and* the new metadata's
    // handler is a core command (step 3). Step 3 wins.
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    registry
        .register(plugin, action.clone(), plugin_metadata(plugin))
        .expect("first registration succeeds");
    let mut metadata = plugin_metadata(plugin);
    metadata.handler = ActionHandlerRef::CoreCommand(CommandKind::Quit);

    assert_eq!(
        registry.register(plugin, action.clone(), metadata),
        Err(RegistryError::InvalidHandler { action })
    );
    assert_eq!(registry.version(), 1);
}

#[test]
fn register_prioritizes_a_duplicate_over_the_cap() {
    // The plugin holds the maximum (step 5) *and* re-registers one of its own
    // references (step 4). Step 4 wins.
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    for index in 0..MAX_PLUGIN_ACTIONS {
        let action = ActionRef::plugin(plugin, &format!("action-{index}"))
            .expect("valid plugin action name");
        registry
            .register(plugin, action, plugin_metadata(plugin))
            .expect("registration below the cap succeeds");
    }
    let held = ActionRef::plugin(plugin, "action-0").expect("valid plugin action name");

    assert_eq!(
        registry.register(plugin, held.clone(), plugin_metadata(plugin)),
        Err(RegistryError::Duplicate { action: held })
    );
    assert_eq!(registry.version(), MAX_PLUGIN_ACTIONS as u64);
}

#[test]
fn a_duplicate_registration_leaves_the_first_entry_untouched() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    let first = plugin_metadata(plugin);
    registry
        .register(plugin, action.clone(), first.clone())
        .expect("first registration succeeds");
    let mut second = plugin_metadata(plugin);
    second.display_name = "Hijacked".to_string();

    assert_eq!(
        registry.register(plugin, action.clone(), second),
        Err(RegistryError::Duplicate {
            action: action.clone()
        })
    );
    assert_eq!(registry.lookup(&action), Some(&first));
}

#[test]
fn register_rejects_a_duplicate_ref_without_bumping_the_version() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    registry
        .register(plugin, action.clone(), plugin_metadata(plugin))
        .expect("first registration succeeds");

    assert_eq!(
        registry.register(plugin, action.clone(), plugin_metadata(plugin)),
        Err(RegistryError::Duplicate {
            action: action.clone()
        })
    );
    assert_eq!(registry.version(), 1);
}

#[test]
fn register_rejects_the_thirty_third_action_of_one_plugin() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    for index in 0..MAX_PLUGIN_ACTIONS {
        let action = ActionRef::plugin(plugin, &format!("action-{index}"))
            .expect("valid plugin action name");
        registry
            .register(plugin, action, plugin_metadata(plugin))
            .expect("registration below the cap succeeds");
    }

    let over_cap = ActionRef::plugin(plugin, "one-too-many").expect("valid plugin action name");
    assert_eq!(
        registry.register(plugin, over_cap, plugin_metadata(plugin)),
        Err(RegistryError::PluginCapExceeded {
            caller: plugin,
            cap: MAX_PLUGIN_ACTIONS,
        })
    );
    assert_eq!(registry.version(), MAX_PLUGIN_ACTIONS as u64);
}

#[test]
fn the_cap_is_counted_per_plugin_not_across_plugins() {
    let mut registry = ActionRegistry::new();
    let full = plugin_id(1);
    for index in 0..MAX_PLUGIN_ACTIONS {
        let action =
            ActionRef::plugin(full, &format!("action-{index}")).expect("valid plugin action name");
        registry
            .register(full, action, plugin_metadata(full))
            .expect("registration below the cap succeeds");
    }

    let other = plugin_id(2);
    let action = ActionRef::plugin(other, "open-status").expect("valid plugin action name");
    assert_eq!(
        registry.register(other, action, plugin_metadata(other)),
        Ok(())
    );
    assert_eq!(registry.version(), MAX_PLUGIN_ACTIONS as u64 + 1);
}

#[test]
fn unregister_removes_a_plugin_action_and_bumps_the_version() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    let metadata = plugin_metadata(plugin);
    registry
        .register(plugin, action.clone(), metadata.clone())
        .expect("registration succeeds");

    assert_eq!(registry.unregister(plugin, &action), Some(metadata));

    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 2);
}

#[test]
fn unregister_of_an_absent_ref_is_none_and_holds_the_version() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let absent = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");

    assert_eq!(registry.unregister(plugin, &absent), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn unregister_never_removes_another_plugins_action() {
    let mut registry = ActionRegistry::new();
    let owner = plugin_id(1);
    let attacker = plugin_id(2);
    let action = ActionRef::plugin(owner, "open-status").expect("valid plugin action name");
    registry
        .register(owner, action.clone(), plugin_metadata(owner))
        .expect("registration succeeds");

    assert_eq!(registry.unregister(attacker, &action), None);

    assert_eq!(registry.lookup(&action), Some(&plugin_metadata(owner)));
    assert_eq!(registry.version(), 1);
}

#[test]
fn unregister_never_removes_a_core_action() {
    let mut registry = ActionRegistry::new();
    let new_pane = ActionRef::core("new-pane").expect("valid core action name");
    let seeded = seeded_metadata(&new_pane);

    assert_eq!(registry.unregister(plugin_id(1), &new_pane), None);

    assert_eq!(registry.lookup(&new_pane), Some(&seeded));
    assert_eq!(registry.version(), 0);
}

#[test]
fn a_second_unregister_of_the_same_ref_is_none_and_holds_the_version() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    registry
        .register(plugin, action.clone(), plugin_metadata(plugin))
        .expect("registration succeeds");
    registry
        .unregister(plugin, &action)
        .expect("first unregister removes the entry");

    assert_eq!(registry.unregister(plugin, &action), None);
    assert_eq!(registry.version(), 2);
}

#[test]
fn a_ref_can_be_registered_again_after_unregister() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    registry
        .register(plugin, action.clone(), plugin_metadata(plugin))
        .expect("registration succeeds");
    registry
        .unregister(plugin, &action)
        .expect("unregister removes the entry");
    let mut renamed = plugin_metadata(plugin);
    renamed.display_name = "Open Status Again".to_string();

    assert_eq!(
        registry.register(plugin, action.clone(), renamed.clone()),
        Ok(())
    );
    assert_eq!(registry.lookup(&action), Some(&renamed));
    assert_eq!(registry.version(), 3);
}

#[test]
fn unregistering_frees_a_slot_under_the_cap() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    for index in 0..MAX_PLUGIN_ACTIONS {
        let action = ActionRef::plugin(plugin, &format!("action-{index}"))
            .expect("valid plugin action name");
        registry
            .register(plugin, action, plugin_metadata(plugin))
            .expect("registration below the cap succeeds");
    }
    let freed = ActionRef::plugin(plugin, "action-0").expect("valid plugin action name");
    registry
        .unregister(plugin, &freed)
        .expect("unregister removes the entry");

    let replacement = ActionRef::plugin(plugin, "replacement").expect("valid plugin action name");
    assert_eq!(
        registry.register(plugin, replacement.clone(), plugin_metadata(plugin)),
        Ok(())
    );
    assert_eq!(
        registry.lookup(&replacement),
        Some(&plugin_metadata(plugin))
    );
    assert_eq!(registry.version(), MAX_PLUGIN_ACTIONS as u64 + 2);
}

#[test]
fn unregister_never_removes_a_user_action() {
    let mut registry = ActionRegistry::new();
    let macro_ref = ActionRef::user("my-macro").expect("valid user action name");

    assert_eq!(registry.unregister(plugin_id(1), &macro_ref), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn list_by_namespace_scopes_to_one_plugin() {
    let mut registry = ActionRegistry::new();
    let first = plugin_id(1);
    let second = plugin_id(2);
    let first_action = ActionRef::plugin(first, "open-status").expect("valid plugin action name");
    let second_action = ActionRef::plugin(second, "open-status").expect("valid plugin action name");
    registry
        .register(first, first_action.clone(), plugin_metadata(first))
        .expect("registration succeeds");
    registry
        .register(second, second_action, plugin_metadata(second))
        .expect("registration succeeds");

    let listed: Vec<&ActionRef> = registry
        .list_by_namespace(ActionNamespace::Plugin(first))
        .map(|(action, _)| action)
        .collect();

    assert_eq!(listed, vec![&first_action]);
}

#[test]
fn a_plugin_registration_leaves_the_core_namespace_untouched() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    registry
        .register(plugin, action, plugin_metadata(plugin))
        .expect("registration succeeds");

    assert_eq!(
        registry.list_by_namespace(ActionNamespace::Core).count(),
        core_action_seeds().len()
    );
}

#[test]
fn default_matches_new() {
    let default = ActionRegistry::default();
    let new = ActionRegistry::new();

    assert_eq!(default.version(), new.version());
    assert_eq!(
        default.list_by_namespace(ActionNamespace::Core).count(),
        new.list_by_namespace(ActionNamespace::Core).count()
    );
}

#[test]
fn registry_error_messages_name_the_offender() {
    let action = ActionRef::core("new-pane").expect("valid core action name");
    let plugin = plugin_id(1);

    assert_eq!(
        RegistryError::Duplicate {
            action: action.clone()
        }
        .to_string(),
        "action core:new-pane is already registered"
    );
    assert_eq!(
        RegistryError::ReservedNamespace { action }.to_string(),
        "action core:new-pane is in a reserved namespace; only plugin: actions may be registered"
    );
    assert_eq!(
        RegistryError::PluginCapExceeded {
            caller: plugin,
            cap: MAX_PLUGIN_ACTIONS,
        }
        .to_string(),
        format!(
            "plugin-01010101-0101-0101-0101-010101010101 already holds the maximum of {MAX_PLUGIN_ACTIONS} actions"
        )
    );
}

#[test]
fn registry_error_ownership_messages_name_the_offender() {
    let action = ActionRef::plugin(plugin_id(1), "open-status").expect("valid plugin action name");

    assert_eq!(
        RegistryError::ForeignNamespace {
            action: action.clone(),
            caller: plugin_id(2),
        }
        .to_string(),
        "action plugin:01010101-0101-0101-0101-010101010101:open-status is not owned by \
         plugin-02020202-0202-0202-0202-020202020202, which may only register in its own namespace"
    );
    assert_eq!(
        RegistryError::NamespaceMismatch {
            action: action.clone()
        }
        .to_string(),
        "action plugin:01010101-0101-0101-0101-010101010101:open-status \
         carries metadata for a different namespace"
    );
    assert_eq!(
        RegistryError::InvalidHandler { action }.to_string(),
        "action plugin:01010101-0101-0101-0101-010101010101:open-status \
         must dispatch through its owning plugin's host call"
    );
}

#[test]
fn registry_error_is_a_recoverable_plugin_failure() {
    let error = RegistryError::Duplicate {
        action: ActionRef::plugin(plugin_id(1), "open-status").expect("valid plugin action name"),
    };

    assert_eq!(error.category(), DomainCategory::Plugin);
    assert_eq!(error.severity(), Severity::Recoverable);
}

#[test]
fn register_strips_control_and_bidi_characters_from_the_plugin_text() {
    let plugin = plugin_id(9);
    let action = ActionRef::plugin(plugin, "status").expect("valid");
    let mut metadata = plugin_metadata(plugin);
    metadata.display_name = "Open\u{7f} Status".to_string();
    metadata.description = "\u{202e}gpj.exe".to_string();
    let mut registry = ActionRegistry::new();

    registry
        .register(plugin, action.clone(), metadata)
        .expect("registered");

    let held = registry.lookup(&action).expect("registered");
    assert_eq!(held.display_name, "Open Status");
    assert_eq!(held.description, "gpj.exe");
}

#[test]
fn register_cuts_the_plugin_text_to_the_reported_text_cap() {
    let plugin = plugin_id(10);
    let action = ActionRef::plugin(plugin, "status").expect("valid");
    let mut metadata = plugin_metadata(plugin);
    metadata.display_name = "a".repeat(crate::text::MAX_REPORTED_TEXT_BYTES + 100);
    metadata.description = "b".repeat(crate::text::MAX_REPORTED_TEXT_BYTES + 1);
    let mut registry = ActionRegistry::new();

    registry
        .register(plugin, action.clone(), metadata)
        .expect("registered");

    let held = registry.lookup(&action).expect("registered");
    assert_eq!(
        held.display_name,
        "a".repeat(crate::text::MAX_REPORTED_TEXT_BYTES)
    );
    assert_eq!(
        held.description,
        "b".repeat(crate::text::MAX_REPORTED_TEXT_BYTES)
    );
}
