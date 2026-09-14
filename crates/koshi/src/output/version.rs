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
pub fn render_client_version(
    client_version: &ClientVersion,
    output_format: OutputFormat,
) -> String {
    match output_format {
        OutputFormat::Json => render_json(client_version),
        OutputFormat::Table => format!("{} {}\n", env!("CARGO_PKG_NAME"), client_version.version),
    }
}

/// Render a `server-version` answer.
#[must_use]
pub fn render_server_versions(
    server_version_rows: &[ServerVersionRow],
    output_format: OutputFormat,
) -> String {
    match output_format {
        OutputFormat::Json => render_json(&server_version_rows),
        OutputFormat::Table => render_table(
            SERVER_VERSION_ROW_HEADERS,
            server_version_rows
                .iter()
                .map(render_server_version_row_cells)
                .collect(),
        ),
    }
}

/// Column headers for [`ServerVersionRow`] listings, matching
/// [`render_server_version_row_cells`].
const SERVER_VERSION_ROW_HEADERS: &[&str] = &["kind", "session", "version"];

/// One [`ServerVersionRow`] as table cells, in
/// [`SERVER_VERSION_ROW_HEADERS`] order.
///
/// The version cell reads `not running` when nothing answered, `unknown` when
/// a server answered without naming a build, and `unreachable` when it could
/// not be asked — that one says why on standard error.
fn render_server_version_row_cells(server_version_row: &ServerVersionRow) -> Vec<String> {
    let server_kind = match server_version_row.server_kind {
        ServerKind::Router => "router",
        ServerKind::Session => "session",
    };
    let build_version = match &server_version_row.build {
        ServerBuild::Running { version } => version.clone(),
        ServerBuild::Unnamed => "unknown".to_string(),
        ServerBuild::NotRunning => "not running".to_string(),
        ServerBuild::Unreachable { .. } => "unreachable".to_string(),
    };
    vec![
        server_kind.to_string(),
        format_optional_cell(server_version_row.session_id.as_ref()),
        build_version,
    ]
}

#[cfg(test)]
mod tests;
