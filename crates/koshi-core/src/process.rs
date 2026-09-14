//! Process lifecycle and spawn types.
//!
//! The PTY layer, the pane layer, and the session close policy all read these
//! types. They are OS-agnostic: how a [`KillPolicy`] maps to actual signals or
//! Win32 calls is the PTY layer's concern.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How to terminate a child process.
///
/// `Graceful` asks the process to exit and waits up to `timeout_duration` before the
/// caller escalates; `Force` kills it immediately; `Tree` kills the whole
/// process group/job, grandchildren included; `GracefulTree` asks the whole
/// group to exit, waits up to `timeout_duration`, then kills the whole group.
///
/// `timeout_duration` serializes as a whole number of seconds (see [`duration_seconds`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KillPolicy {
    /// Request a clean shutdown, allowing up to `timeout_duration` to comply.
    Graceful {
        /// How long to wait for the process to exit on its own.
        #[serde(rename = "timeout", with = "duration_seconds")]
        timeout_duration: Duration,
    },
    /// Kill the process immediately.
    Force,
    /// Kill the entire process tree (group/job), not just the leader.
    Tree,
    /// Request a clean shutdown of the whole process group, allowing up to
    /// `timeout_duration`, then kill the whole group (`killpg` / `TerminateJobObject`).
    GracefulTree {
        /// How long to wait for the process to exit on its own before the
        /// group-kill.
        #[serde(rename = "timeout", with = "duration_seconds")]
        timeout_duration: Duration,
    },
}

impl KillPolicy {
    /// The same kill widened to group scope: `Graceful` becomes
    /// [`GracefulTree`](Self::GracefulTree) with the same timeout, `Force`
    /// becomes [`Tree`](Self::Tree); `Tree` and `GracefulTree` are returned
    /// unchanged.
    #[must_use]
    pub fn apply_tree_scope(self) -> Self {
        match self {
            Self::Graceful { timeout_duration } => Self::GracefulTree { timeout_duration },
            Self::Force => Self::Tree,
            already_tree_scoped => already_tree_scoped,
        }
    }
}

/// The known shells, as [`from_program`](Self::from_program) classifies a
/// program path. The PTY layer picks shell-specific launch behaviour by this
/// kind.
///
/// `Other` carries the lowercased program stem of every other shell.
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
    /// Classify a shell from its program path by its file stem, compared
    /// ASCII-case-insensitively. The stem excludes the last extension on every
    /// platform: `PowerShell.exe` and `pwsh` are both
    /// [`ShellKind::PowerShell`]. Unrecognised programs become
    /// [`ShellKind::Other`] carrying the lowercased stem; a path with no stem
    /// (`""`) or a stem that is not valid UTF-8 yields `Other("")`.
    #[must_use]
    pub fn from_program(program: &Path) -> Self {
        let program_stem = program
            .file_stem()
            .and_then(|program_stem_text| program_stem_text.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match program_stem.as_str() {
            "zsh" => ShellKind::Zsh,
            "bash" => ShellKind::Bash,
            "fish" => ShellKind::Fish,
            "pwsh" | "powershell" => ShellKind::PowerShell,
            "nu" => ShellKind::Nu,
            unrecognized_program_stem => ShellKind::Other(unrecognized_program_stem.to_string()),
        }
    }
}

/// A fully-resolved request to spawn a child process in a PTY.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// The program to execute.
    pub program: PathBuf,
    /// Arguments passed to the program (excluding `argv[0]`).
    #[serde(rename = "args")]
    pub arguments: Vec<String>,
    /// Working directory; `None` inherits the parent's.
    #[serde(rename = "cwd")]
    pub working_directory: Option<PathBuf>,
    /// Environment overrides, sorted for deterministic serialization.
    #[serde(rename = "env")]
    pub environment_variables: BTreeMap<String, String>,
    /// Which shell this spawn targets.
    pub shell_kind: ShellKind,
}

impl SpawnSpec {
    /// Build a spec that launches the platform default interactive shell.
    ///
    /// The program is read from `$SHELL` on Unix and `%COMSPEC%` on Windows,
    /// falling back to `/bin/sh` and `cmd.exe` respectively. A variable that is
    /// set but empty (`SHELL=`) takes the fallback; the program is never an
    /// empty path. `working_directory` and `environment_variables` pass
    /// straight through; `arguments` is empty;
    /// `shell_kind` is [`ShellKind::from_program`] of the chosen program.
    #[must_use]
    pub fn default_shell(
        working_directory: Option<PathBuf>,
        environment_variables: BTreeMap<String, String>,
    ) -> SpawnSpec {
        #[cfg(windows)]
        let program = resolve_shell_program(std::env::var_os("COMSPEC"), "cmd.exe");
        #[cfg(not(windows))]
        let program = resolve_shell_program(std::env::var_os("SHELL"), "/bin/sh");

        SpawnSpec::from_shell_program(program, working_directory, environment_variables)
    }

    /// Build a spec that launches `program` as an interactive shell with no
    /// arguments. `shell_kind` is [`ShellKind::from_program`] of `program`.
    #[must_use]
    pub fn from_shell_program(
        program: PathBuf,
        working_directory: Option<PathBuf>,
        environment_variables: BTreeMap<String, String>,
    ) -> SpawnSpec {
        let shell_kind = ShellKind::from_program(&program);
        SpawnSpec {
            program,
            arguments: Vec::new(),
            working_directory,
            environment_variables,
            shell_kind,
        }
    }
}

/// Pick the shell program path from an environment variable's value: the value
/// when present and non-empty, else `fallback`. A set-but-empty variable
/// (`SHELL=`) takes `fallback`.
fn resolve_shell_program(
    environment_value: Option<std::ffi::OsString>,
    fallback_program: &str,
) -> PathBuf {
    PathBuf::from(
        environment_value
            .filter(|environment_value| !environment_value.is_empty())
            .unwrap_or_else(|| fallback_program.into()),
    )
}

/// A PTY window size in cells.
///
/// Same cell semantics as `geometry::Size`, but a distinct type that does not
/// interchange with a grid `Size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PtySize {
    /// Width in cells (columns).
    #[serde(rename = "cols")]
    pub column_count: u16,
    /// Height in cells (rows).
    #[serde(rename = "rows")]
    pub row_count: u16,
}

/// Serialize a [`Duration`] as a whole number of seconds: a plain unsigned
/// integer with the sub-second part dropped. Deserialization refuses a
/// negative or fractional number.
pub mod duration_seconds {
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
        let seconds = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(seconds))
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
