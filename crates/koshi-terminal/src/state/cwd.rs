//! A working directory reported by the shell via OSC 7.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A working directory reported by the shell via OSC 7: the decoded `path`
/// together with the `host` the shell named in the URI authority.
///
/// The parser stores the report verbatim and makes no local/remote decision.
/// The pane-spawn layer compares `host` to the local machine and refuses to
/// inherit a directory a remote host reported; a shell over SSH reports
/// `file://remote/…`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
