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
    /// A terminal pane backed by a PTY and emulated terminal.
    Terminal,
    /// A plugin pane rendered by an external plugin.
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
    /// Stable id, matching the layout leaf that references this pane. Read-only:
    /// the registry keys records by it, so it is fixed for the record's life.
    id: PaneId,
    /// What backs the pane (terminal or plugin surface). Fixed at creation so a
    /// pane's diagnostics domain never changes underneath it.
    kind: PaneKind,
    /// The pane's display title, if set by the child or explicitly assigned.
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
    /// A fresh `Spawning` record for a terminal-backed pane.
    pub fn new(id: PaneId, created_at: SystemTime) -> Self {
        Self::new_with_kind(id, PaneKind::Terminal, created_at)
    }

    /// A fresh `Spawning` record for a pane backed by `kind`. `kind` is fixed
    /// here and never changes afterward, so the pane's diagnostics domain stays
    /// stable for its whole life.
    pub fn new_with_kind(id: PaneId, kind: PaneKind, created_at: SystemTime) -> Self {
        Self {
            id,
            kind,
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

    /// This pane's stable id, matching its layout leaf and registry key.
    #[must_use]
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// What backs this pane. Fixed at creation.
    #[must_use]
    pub fn kind(&self) -> &PaneKind {
        &self.kind
    }

    /// Where this pane sits in its lifecycle state machine.
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
