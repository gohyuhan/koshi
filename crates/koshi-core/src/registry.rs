//! The live action registry — the runtime's mutable table of every action koshi
//! can perform.
//!
//! [`action`](crate::action) defines what an action reference looks like and
//! ships the built-in set; this module holds the table at run time. The table
//! changes while koshi runs: a plugin load adds its `plugin:<id>:*` entries, an
//! unload removes them.
//!
//! The registry answers one question: given this [`ActionRef`], what do we
//! know about it? Mapping keys to actions is the keymap's job; turning an
//! action into a [`Command`](crate::command::Command) is the resolver's.
//!
//! [`register`](ActionRegistry::register) accepts `plugin:` references only:
//! `core:` is seeded once at [`new`](ActionRegistry::new), and `user:` has no
//! registration path. The reference's namespace, the metadata's namespace, and
//! the handler's target must all name `caller`, and the handler must be the
//! caller's own
//! [`PluginHostCall`](crate::action::ActionHandlerRef::PluginHostCall).
//!
//! [`version`](ActionRegistry::version) counts successful adds and removes.

use std::collections::HashMap;
use std::fmt;

use crate::action::{
    core_action_seeds, ActionHandlerRef, ActionMetadata, ActionNamespace, ActionRef,
};
use crate::error::{DomainCategory, DomainError, Severity};
use crate::ids::PluginId;

/// The number of entries a single plugin may hold in the registry at once.
/// Registration past it is refused.
///
/// The cap counts entries. It does not bound the byte length of a plugin's
/// `display_name` or `description`.
pub const MAX_PLUGIN_ACTIONS: usize = 32;

/// Why an [`ActionRegistry::register`] call was refused. Each variant carries
/// the reference or plugin it rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// The reference is already in the table.
    Duplicate {
        /// The reference that is already registered.
        action: ActionRef,
    },
    /// The reference is a `core:` or `user:` reference.
    ReservedNamespace {
        /// The reference whose namespace is not `plugin:`.
        action: ActionRef,
    },
    /// The reference belongs to a plugin other than the caller.
    ForeignNamespace {
        /// The reference the caller does not own.
        action: ActionRef,
        /// The plugin the caller was authenticated as.
        caller: PluginId,
    },
    /// The metadata's namespace differs from the reference's namespace.
    NamespaceMismatch {
        /// The reference whose metadata disagreed with it.
        action: ActionRef,
    },
    /// The metadata's handler is not the owning plugin's
    /// [`PluginHostCall`](ActionHandlerRef::PluginHostCall).
    InvalidHandler {
        /// The reference whose handler was not its owner's host call.
        action: ActionRef,
    },
    /// The caller already holds [`MAX_PLUGIN_ACTIONS`] actions.
    PluginCapExceeded {
        /// The plugin the caller was authenticated as, which reached its cap.
        caller: PluginId,
        /// The cap that was reached.
        cap: usize,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Duplicate { action } => {
                write!(f, "action {action} is already registered")
            }
            RegistryError::ReservedNamespace { action } => write!(
                f,
                "action {action} is in a reserved namespace; only plugin: actions may be registered"
            ),
            RegistryError::ForeignNamespace { action, caller } => write!(
                f,
                "action {action} is not owned by {caller}, which may only register in its own namespace"
            ),
            RegistryError::NamespaceMismatch { action } => write!(
                f,
                "action {action} carries metadata for a different namespace"
            ),
            RegistryError::InvalidHandler { action } => write!(
                f,
                "action {action} must dispatch through its owning plugin's host call"
            ),
            RegistryError::PluginCapExceeded { caller, cap } => {
                write!(f, "{caller} already holds the maximum of {cap} actions")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

impl DomainError for RegistryError {
    fn category(&self) -> DomainCategory {
        DomainCategory::Plugin
    }

    fn severity(&self) -> Severity {
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
    entries: HashMap<ActionRef, ActionMetadata>,
    /// Successful adds and removes since [`new`](Self::new).
    version: u64,
}

impl ActionRegistry {
    /// Build a registry holding the built-in `core:` actions, at version 0.
    #[must_use]
    pub fn new() -> Self {
        ActionRegistry {
            entries: core_action_seeds().into_iter().collect(),
            version: 0,
        }
    }

    /// Add `caller`'s action to the table and bump [`version`](Self::version).
    ///
    /// `caller` is the plugin the host authenticated. Both `action` and
    /// `metadata` are checked against it: the reference is in `caller`'s
    /// namespace, the metadata repeats that namespace, and the handler is
    /// `caller`'s own [`PluginHostCall`](ActionHandlerRef::PluginHostCall).
    /// The checks run in the order the errors are listed below; the first
    /// failing one is returned.
    ///
    /// # Errors
    /// - [`RegistryError::ReservedNamespace`] if `action` is a `core:` or
    ///   `user:` reference.
    /// - [`RegistryError::ForeignNamespace`] if `action` belongs to a plugin
    ///   other than `caller`.
    /// - [`RegistryError::NamespaceMismatch`] if `metadata.namespace` names a
    ///   different owner than `action` does.
    /// - [`RegistryError::InvalidHandler`] if `metadata.handler` is anything
    ///   other than `caller`'s own host call.
    /// - [`RegistryError::Duplicate`] if `action` is already registered.
    /// - [`RegistryError::PluginCapExceeded`] if `caller` already holds
    ///   [`MAX_PLUGIN_ACTIONS`] actions.
    pub fn register(
        &mut self,
        caller: PluginId,
        action: ActionRef,
        metadata: ActionMetadata,
    ) -> Result<(), RegistryError> {
        // 1. The reference must be in `caller`'s own `plugin:` namespace.
        match action.namespace {
            ActionNamespace::Core | ActionNamespace::User => {
                return Err(RegistryError::ReservedNamespace { action })
            }
            ActionNamespace::Plugin(owner) if owner != caller => {
                return Err(RegistryError::ForeignNamespace { action, caller })
            }
            ActionNamespace::Plugin(_) => {}
        }

        // 2. The metadata must restate the same namespace as the reference.
        if metadata.namespace != action.namespace {
            return Err(RegistryError::NamespaceMismatch { action });
        }

        // 3. The handler must be `caller`'s own host call — no core command, no
        // sequence, and no other plugin's host call.
        if metadata.handler != ActionHandlerRef::PluginHostCall(caller) {
            return Err(RegistryError::InvalidHandler { action });
        }

        // 4. The reference must not already be registered.
        if self.entries.contains_key(&action) {
            return Err(RegistryError::Duplicate { action });
        }

        // 5. `caller` must not already hold the maximum number of entries.
        let held = self
            .entries
            .keys()
            .filter(|registered| registered.namespace == ActionNamespace::Plugin(caller))
            .count();
        if held >= MAX_PLUGIN_ACTIONS {
            return Err(RegistryError::PluginCapExceeded {
                caller,
                cap: MAX_PLUGIN_ACTIONS,
            });
        }

        self.entries.insert(action, metadata);
        self.version += 1;
        Ok(())
    }

    /// Remove one of `caller`'s actions, returning the metadata it held.
    ///
    /// `caller` is the plugin the host authenticated. An `action` in any other
    /// namespace — `core:`, `user:`, or another plugin's — leaves the table
    /// untouched. Returns `None` whenever nothing was removed; the version
    /// bumps only when an entry was.
    pub fn unregister(&mut self, caller: PluginId, action: &ActionRef) -> Option<ActionMetadata> {
        if action.namespace != ActionNamespace::Plugin(caller) {
            return None;
        }
        let metadata = self.entries.remove(action)?;
        self.version += 1;
        Some(metadata)
    }

    /// The metadata of `action`, or `None` when the reference names no entry.
    #[must_use]
    pub fn lookup(&self, action: &ActionRef) -> Option<&ActionMetadata> {
        self.entries.get(action)
    }

    /// Every action in `namespace`, in unspecified order.
    pub fn list_by_namespace(
        &self,
        namespace: ActionNamespace,
    ) -> impl Iterator<Item = (&ActionRef, &ActionMetadata)> + '_ {
        self.entries
            .iter()
            .filter(move |(action, _)| action.namespace == namespace)
    }

    /// How many adds and removes have succeeded since [`new`](Self::new).
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }
}

impl Default for ActionRegistry {
    fn default() -> Self {
        ActionRegistry::new()
    }
}

#[cfg(test)]
pub(crate) mod tests;
