//! Process lifecycle and spawn types.
//!
//! These types live in `koshi-core` so the PTY layer, pane layer, and session
//! close policy all share one definition. They are intentionally cell-agnostic
//! and OS-agnostic: how a [`KillPolicy`] maps to actual signals or Win32 calls
//! is the PTY layer's concern, not this module's.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How to terminate a child process.
///
/// `Graceful` asks the process to exit and waits up to `timeout` before the
/// caller escalates; `Force` kills it immediately; `Tree` kills the whole
/// process group/job so orphaned grandchildren do not linger; `GracefulTree`
/// combines the last two — it asks the whole group to exit, waits up to
/// `timeout`, then group-kills so no descendant is left orphaned.
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
    /// Request a clean shutdown of the whole process group, allowing up to
    /// `timeout`, then group-kill (`killpg` / `TerminateJobObject`) so no
    /// descendant is orphaned.
    GracefulTree {
        /// How long to wait for the process to exit on its own before the
        /// group-kill.
        #[serde(with = "duration_secs")]
        timeout: Duration,
    },
}

impl KillPolicy {
    /// The same kill widened to group scope, so no descendant survives:
    /// `Graceful` becomes [`GracefulTree`](Self::GracefulTree) with the same
    /// timeout, `Force` becomes [`Tree`](Self::Tree); the group-scoped
    /// policies are returned unchanged.
    #[must_use]
    pub fn tree_scoped(self) -> Self {
        match self {
            Self::Graceful { timeout } => Self::GracefulTree { timeout },
            Self::Force => Self::Tree,
            already_tree_scoped => already_tree_scoped,
        }
    }
}

/// The known shells, used to pick shell-specific launch behaviour.
///
/// `Other` carries the raw program name for shells we do not special-case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellKind {
    /// Z shell.
    Zsh,
    /// Bourne-again shell.
    Bash,
    /// Friendly interactive shell.
    Fish,
    /// PowerShell.
    PowerShell,
    /// Nu shell.
    Nu,
    /// Unrecognized shell; carries the lowercased program name.
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

impl SpawnSpec {
    /// Build a spec that launches the platform default interactive shell.
    ///
    /// The program is read from `$SHELL` on Unix and `%COMSPEC%` on Windows,
    /// falling back to `/bin/sh` and `cmd.exe` respectively. A variable that is
    /// set but empty (`SHELL=`) is treated as unset and takes the fallback, so
    /// the program is never an empty path. `cwd` and `env` pass straight through;
    /// `args` is empty.
    #[must_use]
    pub fn default_shell(cwd: Option<PathBuf>, env: BTreeMap<String, String>) -> SpawnSpec {
        #[cfg(windows)]
        let program = shell_program(std::env::var_os("COMSPEC"), "cmd.exe");
        #[cfg(not(windows))]
        let program = shell_program(std::env::var_os("SHELL"), "/bin/sh");

        let shell_kind = ShellKind::from_program(&program);
        SpawnSpec {
            program,
            args: Vec::new(),
            cwd,
            env,
            shell_kind,
        }
    }
}

/// Pick the shell program path from an environment variable's value: the value
/// when present and non-empty, else `fallback`. A set-but-empty variable
/// (`SHELL=`) is treated as unset, so the returned path is never empty. Split out
/// from [`SpawnSpec::default_shell`] so the fallback logic is testable without
/// mutating the process environment.
fn shell_program(env_value: Option<std::ffi::OsString>, fallback: &str) -> PathBuf {
    PathBuf::from(
        env_value
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| fallback.into()),
    )
}

/// A PTY window size in cells.
///
/// Mirrors the cell semantics of `geometry::Size` but is a distinct type so the
/// PTY dimension is never accidentally interchanged with a grid `Size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PtySize {
    /// Width in cells (columns).
    pub cols: u16,
    /// Height in cells (rows).
    pub rows: u16,
}

/// Serialize a [`Duration`] as a whole number of seconds.
///
/// `KillPolicy` timeouts are coarse, so the sub-second part is intentionally
/// dropped on the wire; this keeps the serialized form a plain integer.
pub mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    /// Serialize a [`Duration`] to a whole number of seconds, discarding sub-second precision.
    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_secs())
    }

    /// Deserialize a [`Duration`] from a whole number of seconds.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}

/// How a spawned child ended.
///
/// `ExitCode` carries the process's own exit status; `Signaled` carries the
/// signal number that killed it, for which no exit code exists. The PTY layer
/// reports one of these per child; the runtime maps it onto the session's
/// `Option<i32>` exit code, where a signal becomes `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitStatus {
    /// The child exited on its own with this code (`0` is success by convention).
    ExitCode(i32),
    /// The child was killed by this signal number; it carries no exit code.
    Signaled(i32),
}

#[cfg(test)]
mod tests;
