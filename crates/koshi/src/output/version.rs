//! Renderers for the two version answers: `version`, which reports the
//! program running the command, and `server-version`, which reports each
//! running koshi server.

use super::*;
use crate::version::{ClientVersion, ServerBuild, ServerKind, ServerVersionRow};

/// Render a `version` answer.
///
/// The table form is the line `koshi --version` prints — the program name,
/// a space, the build, and a newline.
#[must_use]
pub fn render_client_version(version: &ClientVersion, format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(version),
        FormatArg::Table => format!("{} {}\n", env!("CARGO_PKG_NAME"), version.version),
    }
}

/// Render a `server-version` answer.
#[must_use]
pub fn render_server_versions(rows: &[ServerVersionRow], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&rows),
        FormatArg::Table => table(
            SERVER_VERSION_ROW_HEADERS,
            rows.iter().map(server_version_row_cells).collect(),
        ),
    }
}

/// Column headers for [`ServerVersionRow`] listings, matching
/// [`server_version_row_cells`].
const SERVER_VERSION_ROW_HEADERS: &[&str] = &["kind", "session", "version"];

/// One [`ServerVersionRow`] as table cells, in
/// [`SERVER_VERSION_ROW_HEADERS`] order.
///
/// The version cell reads `not running` when nothing answered, `unknown` when
/// a server answered without naming a build, and `unreachable` when it could
/// not be asked — that one says why on standard error.
fn server_version_row_cells(row: &ServerVersionRow) -> Vec<String> {
    let kind = match row.kind {
        ServerKind::Router => "router",
        ServerKind::Session => "session",
    };
    let version = match &row.build {
        ServerBuild::Running { version } => version.clone(),
        ServerBuild::Unnamed => "unknown".to_string(),
        ServerBuild::NotRunning => "not running".to_string(),
        ServerBuild::Unreachable { .. } => "unreachable".to_string(),
    };
    vec![kind.to_string(), opt_cell(row.session.as_ref()), version]
}

#[cfg(test)]
mod tests;
