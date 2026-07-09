//! Unit tests for the action registry: the built-in seed load, the namespace
//! guard on registration, duplicate rejection, the per-plugin cap, removal, and
//! the version counter.

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

/// Metadata for a plugin-contributed action. The shape does not matter to the
/// registry, which only stores it; `handler` routes back to the owning plugin.
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

    assert_eq!(registry.register(action.clone(), metadata.clone()), Ok(()));

    assert_eq!(registry.lookup(&action), Some(&metadata));
    assert_eq!(registry.version(), 1);
}

#[test]
fn register_rejects_the_core_namespace() {
    let mut registry = ActionRegistry::new();
    let action = ActionRef::core("take-over").expect("valid core action name");
    let metadata = plugin_metadata(plugin_id(1));

    assert_eq!(
        registry.register(action.clone(), metadata),
        Err(RegistryError::ReservedNamespace { action })
    );
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_rejects_the_user_namespace() {
    let mut registry = ActionRegistry::new();
    let action = ActionRef::user("my-macro").expect("valid user action name");
    let metadata = plugin_metadata(plugin_id(1));

    assert_eq!(
        registry.register(action.clone(), metadata),
        Err(RegistryError::ReservedNamespace { action })
    );
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
        registry.register(action.clone(), metadata),
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
        registry.register(action.clone(), metadata),
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
        registry.register(action.clone(), metadata),
        Err(RegistryError::ForeignHandler {
            action: action.clone(),
            handler: other,
        })
    );
    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn register_accepts_a_plugin_action_that_aliases_a_core_command() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "split-right").expect("valid plugin action name");
    let mut metadata = plugin_metadata(plugin);
    metadata.handler = ActionHandlerRef::CoreCommand(CommandKind::NewPane);

    assert_eq!(registry.register(action.clone(), metadata.clone()), Ok(()));
    assert_eq!(registry.lookup(&action), Some(&metadata));
}

#[test]
fn register_accepts_a_plugin_action_that_fires_a_sequence() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "split-twice").expect("valid plugin action name");
    let new_pane = ActionRef::core("new-pane").expect("valid core action name");
    let mut metadata = plugin_metadata(plugin);
    metadata.handler = ActionHandlerRef::Sequence(vec![new_pane.clone(), new_pane]);

    assert_eq!(registry.register(action.clone(), metadata.clone()), Ok(()));
    assert_eq!(registry.lookup(&action), Some(&metadata));
}

#[test]
fn register_rejects_a_duplicate_ref_without_bumping_the_version() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    registry
        .register(action.clone(), plugin_metadata(plugin))
        .expect("first registration succeeds");

    assert_eq!(
        registry.register(action.clone(), plugin_metadata(plugin)),
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
            .register(action, plugin_metadata(plugin))
            .expect("registration below the cap succeeds");
    }

    let over_cap = ActionRef::plugin(plugin, "one-too-many").expect("valid plugin action name");
    assert_eq!(
        registry.register(over_cap, plugin_metadata(plugin)),
        Err(RegistryError::PluginCapExceeded {
            plugin,
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
            .register(action, plugin_metadata(full))
            .expect("registration below the cap succeeds");
    }

    let other = plugin_id(2);
    let action = ActionRef::plugin(other, "open-status").expect("valid plugin action name");
    assert_eq!(registry.register(action, plugin_metadata(other)), Ok(()));
    assert_eq!(registry.version(), MAX_PLUGIN_ACTIONS as u64 + 1);
}

#[test]
fn unregister_removes_a_plugin_action_and_bumps_the_version() {
    let mut registry = ActionRegistry::new();
    let plugin = plugin_id(1);
    let action = ActionRef::plugin(plugin, "open-status").expect("valid plugin action name");
    let metadata = plugin_metadata(plugin);
    registry
        .register(action.clone(), metadata.clone())
        .expect("registration succeeds");

    assert_eq!(registry.unregister(&action), Some(metadata));

    assert_eq!(registry.lookup(&action), None);
    assert_eq!(registry.version(), 2);
}

#[test]
fn unregister_of_an_absent_ref_is_none_and_holds_the_version() {
    let mut registry = ActionRegistry::new();
    let absent = ActionRef::plugin(plugin_id(1), "open-status").expect("valid plugin action name");

    assert_eq!(registry.unregister(&absent), None);
    assert_eq!(registry.version(), 0);
}

#[test]
fn unregister_never_removes_a_core_action() {
    let mut registry = ActionRegistry::new();
    let new_pane = ActionRef::core("new-pane").expect("valid core action name");

    assert_eq!(registry.unregister(&new_pane), None);

    assert!(registry.lookup(&new_pane).is_some());
    assert_eq!(registry.version(), 0);
}

#[test]
fn unregister_never_removes_a_user_action() {
    let mut registry = ActionRegistry::new();
    let macro_ref = ActionRef::user("my-macro").expect("valid user action name");

    assert_eq!(registry.unregister(&macro_ref), None);
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
        .register(first_action.clone(), plugin_metadata(first))
        .expect("registration succeeds");
    registry
        .register(second_action, plugin_metadata(second))
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
        .register(action, plugin_metadata(plugin))
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
            plugin,
            cap: MAX_PLUGIN_ACTIONS,
        }
        .to_string(),
        format!(
            "plugin-01010101-0101-0101-0101-010101010101 already holds the maximum of {MAX_PLUGIN_ACTIONS} actions"
        )
    );
}

#[test]
fn registry_error_mismatch_messages_name_the_offender() {
    let action = ActionRef::plugin(plugin_id(1), "open-status").expect("valid plugin action name");

    assert_eq!(
        RegistryError::NamespaceMismatch {
            action: action.clone()
        }
        .to_string(),
        "action plugin:01010101-0101-0101-0101-010101010101:open-status \
         carries metadata for a different namespace"
    );
    assert_eq!(
        RegistryError::ForeignHandler {
            action,
            handler: plugin_id(2),
        }
        .to_string(),
        "action plugin:01010101-0101-0101-0101-010101010101:open-status routes to \
         plugin-02020202-0202-0202-0202-020202020202, which does not own it"
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
