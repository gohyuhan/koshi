//! Pane metadata model: the per-pane runtime record the registry owns, and the
//! tag that says what backs a pane.
//!
//! A layout tree holds only a `PaneId` at each leaf; everything else about that
//! pane — what it is, what it ran, where, its lifecycle and timestamps — lives
//! in its [`PaneRecord`] here, so the layout stays pure geometry and runtime
//! state has exactly one owner.

use std::{collections::BTreeMap, path::PathBuf, time::SystemTime};

use serde::{Deserialize, Serialize};
use tile_core::{
    error::DomainCategory,
    ids::{PaneId, PluginId},
    process::SpawnSpec,
};

use crate::error::InvalidTransition;
use crate::pane::{
    lifecycle::{PaneLifecycle, PaneLifecycleEvent},
    policy::{PaneClosePolicy, PaneExitPolicy},
};

/// What backs a pane: an emulated terminal over a PTY, or a plugin-rendered
/// surface. Both are layout leaves with identical split/resize/focus rules; the
/// kind tells the runtime which path drives the pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneKind {
    Terminal,
    Plugin { plugin_id: PluginId },
}

impl PaneKind {
    /// The diagnostics domain a failure on this pane classifies into: a terminal
    /// pane is a `Terminal` failure, a plugin pane a `Plugin` failure — so a
    /// pane-domain error is never mislabelled as a terminal-emulator failure.
    #[must_use]
    pub fn domain_category(&self) -> DomainCategory {
        match self {
            PaneKind::Terminal => DomainCategory::Terminal,
            PaneKind::Plugin { .. } => DomainCategory::Plugin,
        }
    }
}

/// Runtime metadata for a single pane, keyed by `id` in the registry. The
/// layout holds only the id; this record is the one owner of everything else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRecord {
    /// Stable id, matching the layout leaf that references this pane.
    pub id: PaneId,
    /// What backs the pane (terminal or plugin surface).
    pub kind: PaneKind,
    /// Display title, once one has been set or reported.
    pub title: Option<String>,
    /// The process this pane was spawned to run, if any.
    pub command: Option<SpawnSpec>,
    /// Working directory the pane started in, when known.
    pub cwd: Option<PathBuf>,
    /// How a requested close is carried out.
    pub close_policy: PaneClosePolicy,
    /// What happens to the pane when its child exits.
    pub exit_policy: PaneExitPolicy,
    /// Environment overrides applied at spawn, sorted for deterministic output.
    pub env: BTreeMap<String, String>,
    /// Where the pane sits in its lifecycle.
    lifecycle: PaneLifecycle,
    /// When the pane was created.
    pub created_at: SystemTime,
    /// When the pane's child exited, once it has.
    pub exited_at: Option<SystemTime>,
    /// The child's exit code, once it has exited.
    pub exit_code: Option<i32>,
}

impl PaneRecord {
    pub fn new(id: PaneId, created_at: SystemTime) -> Self {
        Self {
            id,
            kind: PaneKind::Terminal,
            title: None,
            command: None,
            cwd: None,
            close_policy: PaneClosePolicy::default(),
            exit_policy: PaneExitPolicy::default(),
            env: BTreeMap::new(),
            lifecycle: PaneLifecycle::Spawning,
            created_at,
            exited_at: None,
            exit_code: None,
        }
    }

    pub fn lifecycle(&self) -> &PaneLifecycle {
        &self.lifecycle
    }

    /// Apply a lifecycle `event`, advancing the pane's state, or return
    /// [`InvalidTransition`] if the move is illegal from the current state.
    /// The pane is the sole owner of its lifecycle (the field is private), so
    /// this is the only way to drive it; the caller decides whether a rejected
    /// event is an expected no-op to ignore or a fault to report.
    pub fn update_lifecycle(&mut self, event: PaneLifecycleEvent) -> Result<(), InvalidTransition> {
        self.lifecycle = self.lifecycle.transition(event, self.kind.clone())?;
        Ok(())
    }
}
