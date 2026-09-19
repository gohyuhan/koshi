//! Pane metadata: the per-pane runtime record the registry owns, and the tag
//! that says what backs a pane.
//!
//! A layout tree holds only a `PaneId` at each leaf. [`PaneRecord`] holds
//! everything else about that pane: its kind, its spawn specification, its
//! working directory, its lifecycle state and its timestamps.

use std::{path::PathBuf, time::SystemTime};

use koshi_core::{
    error::DomainCategory,
    ids::{PaneId, PluginId},
    process::SpawnSpec,
};
use serde::{Deserialize, Serialize};

use crate::error::InvalidTransitionError;
use crate::pane::{
    lifecycle::{PaneLifecycle, PaneLifecycleEvent},
    policy::{PaneClosePolicy, PaneExitPolicy},
};

/// What backs a pane: an emulated terminal over a PTY, or a surface that a
/// plugin renders. The kind tells the runtime which path drives the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneKind {
    /// A terminal pane backed by a PTY and emulated terminal.
    Terminal,
    /// A plugin pane rendered by an external plugin.
    Plugin {
        /// The plugin that renders the pane.
        plugin_id: PluginId,
    },
}

impl PaneKind {
    /// The diagnostics domain for a failure on this pane. A terminal pane
    /// reports `Terminal`. A plugin pane reports `Plugin`.
    #[must_use]
    pub(crate) fn domain_category(&self) -> DomainCategory {
        match self {
            PaneKind::Terminal => DomainCategory::Terminal,
            PaneKind::Plugin { .. } => DomainCategory::Plugin,
        }
    }
}

/// Runtime metadata for a single pane. The registry keys the record by
/// `pane_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRecord {
    /// The stable pane id, which matches the layout leaf that references this
    /// pane. The pane id never changes.
    pane_id: PaneId,
    /// What backs the pane. The pane kind is set at creation and never changes.
    pane_kind: PaneKind,
    /// The process spawn specification, when the pane has one.
    pub spawn_spec: Option<SpawnSpec>,
    /// The working directory that the pane starts in, when it is known.
    pub working_directory: Option<PathBuf>,
    /// How the pane carries out a requested close.
    pub close_policy: PaneClosePolicy,
    /// What happens to the pane when its child process ends.
    pub exit_policy: PaneExitPolicy,
    /// Where the pane sits in its lifecycle.
    lifecycle: PaneLifecycle,
    /// The time when the pane was created. It never changes.
    created_at: SystemTime,
}

impl PaneRecord {
    /// A fresh `Spawning` record for a terminal-backed pane.
    pub fn from_terminal_pane(pane_id: PaneId, created_at: SystemTime) -> Self {
        Self::from_pane_kind(pane_id, PaneKind::Terminal, created_at)
    }

    /// A fresh `Spawning` record for a pane that `pane_kind` backs. The pane
    /// kind never changes afterwards.
    pub fn from_pane_kind(pane_id: PaneId, pane_kind: PaneKind, created_at: SystemTime) -> Self {
        Self {
            pane_id,
            pane_kind,
            spawn_spec: None,
            working_directory: None,
            close_policy: PaneClosePolicy::default(),
            exit_policy: PaneExitPolicy::default(),
            lifecycle: PaneLifecycle::Spawning,
            created_at,
        }
    }

    /// The stable pane id. It matches the layout leaf and the registry key.
    #[must_use]
    pub fn get_pane_id(&self) -> PaneId {
        self.pane_id
    }

    /// What backs this pane. The pane kind is set at creation and never changes.
    #[must_use]
    pub fn get_pane_kind(&self) -> &PaneKind {
        &self.pane_kind
    }

    /// The time this pane was created. It is set at creation and never
    /// changes.
    #[must_use]
    pub fn get_created_at(&self) -> SystemTime {
        self.created_at
    }

    /// Where this pane sits in its lifecycle state machine.
    pub fn get_lifecycle(&self) -> &PaneLifecycle {
        &self.lifecycle
    }

    /// Applies a lifecycle `lifecycle_event` and advances the pane's state. Returns
    /// [`InvalidTransitionError`] when the step is illegal from the current state,
    /// and leaves the state unchanged. This is the only way to change
    /// `lifecycle`.
    pub fn update_lifecycle(
        &mut self,
        lifecycle_event: PaneLifecycleEvent,
    ) -> Result<(), InvalidTransitionError> {
        self.lifecycle = self.lifecycle.transition(lifecycle_event, self.pane_kind)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
