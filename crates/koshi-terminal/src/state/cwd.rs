//! A working directory reported by the shell via OSC 7.

use std::path::{Path, PathBuf};

/// A working directory reported by the shell via OSC 7: the decoded `path`
/// together with the `host` the shell named in the URI authority.
///
/// The host is kept rather than discarded so the pane-spawn layer can compare
/// it to the local machine and refuse to inherit a directory reported from a
/// *remote* host — e.g. a shell running over SSH reports `file://remote/…`, and
/// opening that path on the local machine would land in the wrong place. The
/// parser stores the report verbatim and makes no local/remote decision; that
/// admission check belongs at the spawn layer that owns the new pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportedCwd {
    /// The URI authority (the part between `//` and the path), or `None` when
    /// it was empty (`file:///path`). `localhost` and the local machine's own
    /// hostname both denote the local machine.
    pub(in crate::state) host: Option<String>,
    /// The decoded working-directory path.
    pub(in crate::state) path: PathBuf,
}

impl ReportedCwd {
    /// The host the shell named, or `None` for an empty authority.
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// The decoded working-directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
