//! KDL parsing entry point. Wraps the `kdl` crate's document parser and attaches
//! the config file path to any syntax error as a [`ConfigParseDiagnostic`].

use std::path::Path;

use kdl::KdlDocument;

use crate::error::ConfigParseDiagnostic;

#[cfg(test)]
mod tests;

/// Parses `source` — the already-read contents of the config file at `path` —
/// into a [`KdlDocument`]. Does no file I/O: discovery and reading happen in
/// the caller.
///
/// # Errors
/// Returns a [`ConfigParseDiagnostic`] carrying `path` and the span-tagged
/// KDL error for pretty rendering when `source` is not valid KDL syntax.
pub fn parse_kdl(path: &Path, source: &str) -> Result<KdlDocument, ConfigParseDiagnostic> {
    source
        .parse::<KdlDocument>()
        .map_err(|err| ConfigParseDiagnostic::new(path, err))
}
