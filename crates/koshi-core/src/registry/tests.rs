//! Unit tests for the action registry: the built-in seed load, the ownership
//! checks that gate registration and removal, the handler restriction, the
//! per-plugin cap, and the version counter.

use super::*;

use crate::action::{
    ActionHandlerRef, ActionMetadata, ActionRef, ActionScope, ActionStatus, TargetKind,
};
use crate::command::CommandKind;
use uuid::Uuid;

/// A plugin id built from a fixed uuid, so two calls with the same byte yield
/// the same plugin and different bytes yield different plugins.
fn plugin_id(byte: u8) -> PluginId {
    PluginId::from_uuid(Uuid::from_bytes([byte; 16]))
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
        args_schema: None,
        handler: ActionHandlerRef::PluginHostCall(plugin),
        status: ActionStatus::Available,
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

    assert!(registry.lookup(&action).is_some());
    assert_eq!(registry.version(), 1);
}

#[test]
fn unregister_never_removes_a_core_action() {
    let mut registry = ActionRegistry::new();
    let new_pane = ActionRef::core("new-pane").expect("valid core action name");

    assert_eq!(registry.unregister(plugin_id(1), &new_pane), None);

    assert!(registry.lookup(&new_pane).is_some());
    assert_eq!(registry.version(), 0);
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
