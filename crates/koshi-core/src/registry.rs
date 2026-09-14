//! The live action registry — the runtime's mutable table of every action koshi
//! can perform.
//!
//! [`action`](crate::action) defines what an action reference looks like and
//! ships the built-in set; this module holds the table at run time. The table
//! changes while koshi runs: a plugin load adds its `plugin:<id>:*` entries, an
//! unload removes them.
//!
//! The registry answers one question: given this [`ActionReference`], what do we
//! know about it? Mapping keys to actions is the keymap's job; turning an
//! action into a [`Command`](crate::command::Command) is the resolver's.
//!
//! [`register_action`](ActionRegistry::register_action) accepts `plugin:` references only:
//! `core:` is seeded once at [`new`](ActionRegistry::new), and `user:` has no
//! registration path. The reference's namespace, the metadata's namespace, and
//! the handler's target must all name the caller plugin, and the handler must
//! be that plugin's own
//! [`PluginHostCall`](crate::action::ActionHandlerReference::PluginHostCall).
//!
//! The registry version counts successful adds and removes.

use std::collections::HashMap;
use std::fmt;

use crate::action::{
    build_core_action_seeds, ActionHandlerReference, ActionMetadata, ActionNamespace,
    ActionReference,
};
use crate::error::{DomainCategory, DomainError, Severity};
use crate::ids::PluginId;
use crate::text::sanitize_reported_text;

/// The number of entries a single plugin may hold in the registry at once.
/// Registration past it is refused.
///
/// The cap counts entries. `display_name` and `description` are bounded
/// separately, by [`crate::text::sanitize_reported_text`] in
/// [`ActionRegistry::register_action`].
pub const MAX_PLUGIN_ACTION_COUNT: usize = 32;

/// Why an [`ActionRegistry::register_action`] call was refused. Each variant carries
/// the reference or plugin it rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// The reference is already in the table.
    Duplicate {
        /// The reference that is already registered.
        action_reference: ActionReference,
    },
    /// The reference is a `core:` or `user:` reference.
    ReservedNamespace {
        /// The reference whose namespace is not `plugin:`.
        action_reference: ActionReference,
    },
    /// The reference belongs to a plugin other than the caller.
    ForeignNamespace {
        /// The reference the caller plugin does not own.
        action_reference: ActionReference,
        /// The plugin identifier the host authenticated.
        caller_plugin_id: PluginId,
    },
    /// The metadata's namespace differs from the reference's namespace.
    NamespaceMismatch {
        /// The reference whose metadata disagreed with it.
        action_reference: ActionReference,
    },
    /// The metadata's handler is not the owning plugin's
    /// [`PluginHostCall`](ActionHandlerReference::PluginHostCall).
    InvalidHandler {
        /// The reference whose handler was not its owner's host call.
        action_reference: ActionReference,
    },
    /// The caller already holds [`MAX_PLUGIN_ACTION_COUNT`] actions.
    PluginActionLimitExceeded {
        /// The plugin identifier that reached its action limit.
        caller_plugin_id: PluginId,
        /// The maximum number of actions allowed for one plugin.
        maximum_action_count: usize,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Duplicate { action_reference } => {
                write!(f, "action {action_reference} is already registered")
            }
            RegistryError::ReservedNamespace { action_reference } => write!(
                f,
                "action {action_reference} is in a reserved namespace; only plugin: actions may be registered"
            ),
            RegistryError::ForeignNamespace {
                action_reference,
                caller_plugin_id,
            } => write!(
                f,
                "action {action_reference} is not owned by {caller_plugin_id}, which may only register in its own namespace"
            ),
            RegistryError::NamespaceMismatch { action_reference } => write!(
                f,
                "action {action_reference} carries metadata for a different namespace"
            ),
            RegistryError::InvalidHandler { action_reference } => write!(
                f,
                "action {action_reference} must dispatch through its owning plugin's host call"
            ),
            RegistryError::PluginActionLimitExceeded {
                caller_plugin_id,
                maximum_action_count,
            } => {
                write!(f, "{caller_plugin_id} already holds the maximum of {maximum_action_count} actions")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

impl DomainError for RegistryError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Plugin
    }

    fn get_severity(&self) -> Severity {
        Severity::Recoverable
    }
}

/// Every action koshi can perform, keyed by reference.
///
/// Built with [`new`](ActionRegistry::new), which loads the built-in `core:`
/// table. Plugins add and remove their own entries on top of it.
#[derive(Debug)]
pub struct ActionRegistry {
    /// Each known action and what the runtime knows about it.
    action_metadata_by_reference: HashMap<ActionReference, ActionMetadata>,
    /// Successful adds and removes since [`new`](Self::new).
    registry_version: u64,
}

impl ActionRegistry {
    /// Build a registry holding the built-in `core:` actions, at version 0.
    #[must_use]
    pub fn new() -> Self {
        ActionRegistry {
            action_metadata_by_reference: build_core_action_seeds().into_iter().collect(),
            registry_version: 0,
        }
    }

    /// Add `caller_plugin_id`'s action to the table and bump the registry version.
    ///
    /// `caller_plugin_id` is the plugin the host authenticated. Both
    /// `action_reference` and `action_metadata` are checked against it: the
    /// reference is in `caller_plugin_id`'s namespace, the metadata repeats
    /// that namespace, and the handler is `caller_plugin_id`'s own
    /// [`PluginHostCall`](ActionHandlerReference::PluginHostCall).
    /// The checks run in the order the errors are listed below; the first
    /// failing one is returned.
    ///
    /// # Errors
    /// - [`RegistryError::ReservedNamespace`] if `action_reference` is a `core:` or
    ///   `user:` reference.
    /// - [`RegistryError::ForeignNamespace`] if `action_reference` belongs to a plugin
    ///   other than `caller_plugin_id`.
    /// - [`RegistryError::NamespaceMismatch`] if `action_metadata.namespace` names a
    ///   different owner than `action_reference` does.
    /// - [`RegistryError::InvalidHandler`] if `action_metadata.handler` is anything
    ///   other than `caller_plugin_id`'s own host call.
    /// - [`RegistryError::Duplicate`] if `action_reference` is already registered.
    /// - [`RegistryError::PluginActionLimitExceeded`] if `caller_plugin_id` already holds
    ///   [`MAX_PLUGIN_ACTION_COUNT`] actions.
    pub fn register_action(
        &mut self,
        caller_plugin_id: PluginId,
        action_reference: ActionReference,
        mut action_metadata: ActionMetadata,
    ) -> Result<(), RegistryError> {
        // 1. The reference must be in `caller_plugin_id`'s own `plugin:` namespace.
        match action_reference.namespace {
            ActionNamespace::Core | ActionNamespace::User => {
                return Err(RegistryError::ReservedNamespace { action_reference })
            }
            ActionNamespace::Plugin(action_owner_plugin_id)
                if action_owner_plugin_id != caller_plugin_id =>
            {
                return Err(RegistryError::ForeignNamespace {
                    action_reference,
                    caller_plugin_id,
                })
            }
            ActionNamespace::Plugin(_) => {}
        }

        // 2. The metadata must restate the same namespace as the reference.
        if action_metadata.namespace != action_reference.namespace {
            return Err(RegistryError::NamespaceMismatch { action_reference });
        }

        // 3. The handler must be `caller_plugin_id`'s own host call — no core command, no
        // sequence, and no other plugin's host call.
        if action_metadata.handler != ActionHandlerReference::PluginHostCall(caller_plugin_id) {
            return Err(RegistryError::InvalidHandler { action_reference });
        }

        // 4. The reference must not already be registered.
        if self
            .action_metadata_by_reference
            .contains_key(&action_reference)
        {
            return Err(RegistryError::Duplicate { action_reference });
        }

        // 5. `caller_plugin_id` must not already hold the maximum number of entries.
        let registered_action_count = self
            .action_metadata_by_reference
            .keys()
            .filter(|registered_action_reference| {
                registered_action_reference.namespace == ActionNamespace::Plugin(caller_plugin_id)
            })
            .count();
        if registered_action_count >= MAX_PLUGIN_ACTION_COUNT {
            return Err(RegistryError::PluginActionLimitExceeded {
                caller_plugin_id,
                maximum_action_count: MAX_PLUGIN_ACTION_COUNT,
            });
        }

        // 6. `display_name` and `description` are plugin-supplied text, cut to
        // `MAX_REPORTED_TEXT_BYTE_COUNT` with control, bidi-control and tag
        // characters removed.
        action_metadata.display_name = sanitize_reported_text(&action_metadata.display_name);
        action_metadata.description = sanitize_reported_text(&action_metadata.description);

        self.action_metadata_by_reference
            .insert(action_reference, action_metadata);
        self.registry_version += 1;
        Ok(())
    }

    /// Remove one of `caller_plugin_id`'s actions, returning the metadata it held.
    ///
    /// `caller_plugin_id` is the plugin the host authenticated. An
    /// `action_reference` in any other
    /// namespace — `core:`, `user:`, or another plugin's — leaves the table
    /// untouched. Returns `None` whenever nothing was removed; the version
    /// bumps only when an entry was.
    pub fn remove_action(
        &mut self,
        caller_plugin_id: PluginId,
        action_reference: &ActionReference,
    ) -> Option<ActionMetadata> {
        if action_reference.namespace != ActionNamespace::Plugin(caller_plugin_id) {
            return None;
        }
        let action_metadata = self.action_metadata_by_reference.remove(action_reference)?;
        self.registry_version += 1;
        Some(action_metadata)
    }

    /// The metadata of `action_reference`, or `None` when the reference names no entry.
    #[must_use]
    pub fn find_action_metadata(
        &self,
        action_reference: &ActionReference,
    ) -> Option<&ActionMetadata> {
        self.action_metadata_by_reference.get(action_reference)
    }

    /// Every action in `action_namespace`, in unspecified order.
    pub fn list_actions_by_namespace(
        &self,
        action_namespace: ActionNamespace,
    ) -> impl Iterator<Item = (&ActionReference, &ActionMetadata)> + '_ {
        self.action_metadata_by_reference
            .iter()
            .filter(move |(action_reference, _)| action_reference.namespace == action_namespace)
    }

    /// How many adds and removes have succeeded since [`new`](Self::new).
    #[must_use]
    pub fn get_registry_version(&self) -> u64 {
        self.registry_version
    }
}

impl Default for ActionRegistry {
    fn default() -> Self {
        ActionRegistry::new()
    }
}

#[cfg(test)]
pub(crate) mod tests;
