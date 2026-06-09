//! Process lifecycle and spawn types (FND-019).
//!
//! These types live in `tile-core` so the PTY layer, pane layer, and session
//! close policy all share one definition instead of redefining them per crate
//! (which would invert the dependency layering). They are deliberately
//! cell-agnostic and OS-agnostic: how a [`KillPolicy`] maps to actual signals
//! or Win32 calls is the PTY layer's concern, not this module's.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How to terminate a child process.
///
/// `Graceful` asks the process to exit and waits up to `timeout` before the
/// caller escalates; `Force` kills it immediately; `Tree` kills the whole
/// process group/job so orphaned grandchildren do not linger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KillPolicy {
    /// Request a clean shutdown, allowing up to `timeout` to comply.
    Graceful {
        /// How long to wait for the process to exit on its own.
        #[serde(with = "duration_secs")]
        timeout: Duration,
    },
    /// Kill the process immediately.
    Force,
    /// Kill the entire process tree (group/job), not just the leader.
    Tree,
}

/// The known shells, used to pick shell-specific launch behaviour.
///
/// `Other` carries the raw program name for shells we do not special-case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
    PowerShell,
    Nu,
    Other(String),
}

impl ShellKind {
    /// Classify a shell from its program path by inspecting the file stem
    /// (case-insensitive). The `.exe` suffix on Windows is ignored because
    /// `file_stem` strips it. Unrecognised programs become [`ShellKind::Other`]
    /// carrying the lowercased stem.
    #[must_use]
    pub fn from_program(program: &Path) -> Self {
        let stem = program
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match stem.as_str() {
            "zsh" => ShellKind::Zsh,
            "bash" => ShellKind::Bash,
            "fish" => ShellKind::Fish,
            "pwsh" | "powershell" => ShellKind::PowerShell,
            "nu" => ShellKind::Nu,
            other => ShellKind::Other(other.to_string()),
        }
    }
}

/// A fully-resolved request to spawn a child process in a PTY.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// The program to execute.
    pub program: PathBuf,
    /// Arguments passed to the program (excluding `argv[0]`).
    pub args: Vec<String>,
    /// Working directory; `None` inherits the parent's.
    pub cwd: Option<PathBuf>,
    /// Environment overrides, sorted for deterministic serialization.
    pub env: BTreeMap<String, String>,
    /// Which shell this spawn targets.
    pub shell_kind: ShellKind,
}

/// A PTY window size in cells.
///
/// Mirrors the cell semantics of `geometry::Size` but is a distinct type so the
/// PTY dimension is never accidentally interchanged with a grid `Size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

/// Serialize a [`Duration`] as a whole number of seconds.
///
/// `KillPolicy` timeouts are coarse, so the sub-second part is intentionally
/// dropped on the wire; this keeps the serialized form a plain integer.
mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}

#[cfg(test)]
mod tests;
