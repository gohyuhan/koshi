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
    let server_listing_rows: Vec<ServerListingRow> = server_version_rows
        .iter()
        .map(build_server_listing_row)
        .collect();
    match output_format {
        OutputFormat::Json => render_json(&server_listing_rows),
        OutputFormat::Table => render_table(
            SERVER_VERSION_ROW_HEADERS,
            server_listing_rows
                .iter()
                .map(render_server_version_row_cells)
                .collect(),
        ),
    }
}

/// One koshi server as both formats of `server-version` print it.
#[derive(Serialize)]
struct ServerListingRow {
    /// Which server the row is about: `router` or `session`.
    server_kind: &'static str,
    /// The session the row is about, as `session-<uuid>`; `None` on the
    /// router's row.
    session_id: Option<String>,
    /// What asking the server produced.
    #[serde(flatten)]
    build: ServerBuildReport,
}

/// What asking one koshi server for its build produced. JSON writes the case
/// as `"state"`: `"running"` with `"version"`, `"not running"`, or
/// `"unreachable"` with `"detail"`.
#[derive(Serialize)]
#[serde(tag = "state")]
enum ServerBuildReport {
    /// The server answered. `version` is the build it named, e.g. `0.2.0`,
    /// or `None` from a server too old to name its build.
    #[serde(rename = "running")]
    Running {
        /// The build the server named, or `None` when it named none.
        version: Option<String>,
    },
    /// Nothing is listening there.
    #[serde(rename = "not running")]
    NotRunning,
    /// The server could not be asked.
    #[serde(rename = "unreachable")]
    Unreachable {
        /// What went wrong, as the caller would have been told.
        detail: String,
    },
}

/// One [`ServerVersionRow`] as both formats print it.
fn build_server_listing_row(server_version_row: &ServerVersionRow) -> ServerListingRow {
    ServerListingRow {
        server_kind: match server_version_row.server_kind {
            ServerKind::Router => "router",
            ServerKind::Session => "session",
        },
        session_id: server_version_row
            .session_id
            .map(|session_id| session_id.to_string()),
        build: match &server_version_row.build {
            ServerBuild::Running { version } => ServerBuildReport::Running {
                version: Some(version.clone()),
            },
            ServerBuild::Unnamed => ServerBuildReport::Running { version: None },
            ServerBuild::NotRunning => ServerBuildReport::NotRunning,
            ServerBuild::Unreachable { detail } => ServerBuildReport::Unreachable {
                detail: detail.clone(),
            },
        },
    }
}

/// Column headers for [`ServerListingRow`] listings, matching
/// [`render_server_version_row_cells`].
const SERVER_VERSION_ROW_HEADERS: &[&str] = &["kind", "session", "version"];

/// One [`ServerListingRow`] as table cells, in
/// [`SERVER_VERSION_ROW_HEADERS`] order.
///
/// The version cell reads `unknown` when a server answered without naming a
/// build, `not running` when nothing answered, and `unreachable` when it could
/// not be asked — that one says why on standard error.
fn render_server_version_row_cells(server_listing_row: &ServerListingRow) -> Vec<String> {
    let build_version = match &server_listing_row.build {
        ServerBuildReport::Running {
            version: Some(version),
        } => version.clone(),
        ServerBuildReport::Running { version: None } => "unknown".to_string(),
        ServerBuildReport::NotRunning => "not running".to_string(),
        ServerBuildReport::Unreachable { .. } => "unreachable".to_string(),
    };
    vec![
        server_listing_row.server_kind.to_string(),
        format_optional_cell(server_listing_row.session_id.as_ref()),
        build_version,
    ]
}

#[cfg(test)]
mod tests;
