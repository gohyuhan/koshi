//! Pane metadata: the per-pane runtime record the registry owns, and the tag
//! that says what backs a pane.
//!
//! A layout tree holds only a `PaneId` at each leaf. [`PaneRecord`] holds
//! everything else about that pane: its kind, its command, its working
//! directory, its lifecycle state and its timestamps.

use std::{collections::BTreeMap, path::PathBuf, time::SystemTime};

use koshi_core::{
    error::DomainCategory,
    ids::{PaneId, PluginId},
    process::SpawnSpec,
};
use serde::{Deserialize, Serialize};

use crate::error::InvalidTransition;
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
    pub fn domain_category(&self) -> DomainCategory {
        match self {
            PaneKind::Terminal => DomainCategory::Terminal,
            PaneKind::Plugin { .. } => DomainCategory::Plugin,
        }
    }
}

/// Runtime metadata for a single pane. The registry keys the record by `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRecord {
    /// The stable id, which matches the layout leaf that references this pane.
    /// The id never changes.
    id: PaneId,
    /// What backs the pane. The kind is set at creation and never changes.
    kind: PaneKind,
    /// The process that the pane runs, when the pane has one.
    pub command: Option<SpawnSpec>,
    /// The working directory that the pane starts in, when it is known.
    pub cwd: Option<PathBuf>,
    /// How the pane carries out a requested close.
    pub close_policy: PaneClosePolicy,
    /// What happens to the pane when its child process ends.
    pub exit_policy: PaneExitPolicy,
    /// The environment overrides that apply at spawn, in name order.
    pub env: BTreeMap<String, String>,
    /// Where the pane sits in its lifecycle.
    lifecycle: PaneLifecycle,
    /// The time when the pane was created.
    pub created_at: SystemTime,
}

impl PaneRecord {
    /// A fresh `Spawning` record for a terminal-backed pane.
    pub fn new(id: PaneId, created_at: SystemTime) -> Self {
        Self::new_with_kind(id, PaneKind::Terminal, created_at)
    }

    /// A fresh `Spawning` record for a pane that `kind` backs. The kind never
    /// changes afterwards.
    pub fn new_with_kind(id: PaneId, kind: PaneKind, created_at: SystemTime) -> Self {
        Self {
            id,
            kind,
            command: None,
            cwd: None,
            close_policy: PaneClosePolicy::default(),
            exit_policy: PaneExitPolicy::default(),
            env: BTreeMap::new(),
            lifecycle: PaneLifecycle::Spawning,
            created_at,
        }
    }

    /// The stable id of this pane. It matches the layout leaf and the registry
    /// key.
    #[must_use]
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// What backs this pane. The kind is set at creation and never changes.
    #[must_use]
    pub fn kind(&self) -> &PaneKind {
        &self.kind
    }

    /// Where this pane sits in its lifecycle state machine.
    pub fn lifecycle(&self) -> &PaneLifecycle {
        &self.lifecycle
    }

    /// Applies a lifecycle `event` and advances the pane's state. Returns
    /// [`InvalidTransition`] when the step is illegal from the current state,
    /// and leaves the state unchanged. This is the only way to change
    /// `lifecycle`.
    pub fn update_lifecycle(&mut self, event: PaneLifecycleEvent) -> Result<(), InvalidTransition> {
        self.lifecycle = self.lifecycle.transition(event, self.kind)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
