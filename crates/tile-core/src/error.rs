//! Error categories and severity. Every crate's domain error classifies into a
//! shared [`DomainCategory`] and [`Severity`], so observability and
//! diagnostics can reason about failures uniformly without knowing concrete types.

use serde::{Deserialize, Serialize};

/// The domain a failure originated from. One variant per typed-error domain;
/// this is classification only and carries no payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DomainCategory {
    Config,
    Cli,
    Ipc,
    Pty,
    Terminal,
    Layout,
    Plugin,
    Storage,
}

impl std::fmt::Display for DomainCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DomainCategory::Config => f.write_str("config"),
            DomainCategory::Cli => f.write_str("cli"),
            DomainCategory::Ipc => f.write_str("ipc"),
            DomainCategory::Pty => f.write_str("pty"),
            DomainCategory::Terminal => f.write_str("terminal"),
            DomainCategory::Layout => f.write_str("layout"),
            DomainCategory::Plugin => f.write_str("plugin"),
            DomainCategory::Storage => f.write_str("storage"),
        }
    }
}

/// How far a failure propagates. Ordered from least to most fatal, so callers
/// can compare (`severity >= Severity::SessionFatal`) to decide containment:
/// a pane or plugin failure stays `Recoverable` and must not crash the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    /// Contained; the session keeps running (pane failed, plugin failed,
    /// command rejected).
    Recoverable,
    /// A single client must tear down (renderer/input backend failed).
    ClientFatal,
    /// Core session state is corrupted; the session cannot continue.
    SessionFatal,
    /// The runtime process cannot continue (failed to initialize).
    ProcessFatal,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Recoverable => f.write_str("recoverable"),
            Severity::ClientFatal => f.write_str("client-fatal"),
            Severity::SessionFatal => f.write_str("session-fatal"),
            Severity::ProcessFatal => f.write_str("process-fatal"),
        }
    }
}

/// Implemented by every crate's domain error so a failure can be classified
/// without knowing its concrete type. The aggregate `TileError` (in
/// `tile-observability`) delegates through this trait.
pub trait DomainError {
    /// Which domain the failure belongs to.
    fn category(&self) -> DomainCategory;
    /// How far the failure propagates.
    fn severity(&self) -> Severity;
}

#[cfg(test)]
mod tests;
