//! Renderers for the `remote` answers: the listing of every server this
//! machine has saved, the line naming a server that was saved or changed, the
//! line saying nothing was saved, the line naming the server a forget dropped,
//! and the line naming the server whose secret was replaced.
//!
//! A saved server's secret is not a field of the listing row, so no format
//! this module renders can print one.

use super::*;
use koshi_ipc::remote_servers::SavedServer;

/// Render a `remote list` answer.
#[must_use]
pub fn render_remote_list(saved_servers: &[SavedServer], output_format: OutputFormat) -> String {
    let remote_server_rows: Vec<RemoteServerRow> =
        saved_servers.iter().map(build_remote_server_row).collect();
    match output_format {
        OutputFormat::Json => render_json(&remote_server_rows),
        OutputFormat::Table => render_table(
            REMOTE_HEADERS,
            remote_server_rows
                .iter()
                .map(render_remote_server_row_cells)
                .collect(),
        ),
    }
}

/// Render a `remote forget` answer: the one line naming the server that is no
/// longer saved.
#[must_use]
pub fn render_remote_forget(server_address: &str) -> String {
    format!("forgot {server_address}.\n")
}

/// Render a `remote set-secret` answer: the one line naming the server whose
/// secret this machine now presents.
#[must_use]
pub fn render_remote_secret(server_address: &str) -> String {
    format!("the secret for {server_address} was replaced.\n")
}

/// Render a `remote new` answer: the one line naming the server this machine
/// now holds.
#[must_use]
pub fn render_remote_saved(saved_server: &SavedServer) -> String {
    render_settled_server_line("saved", saved_server)
}

/// Render a `remote edit` answer: the one line naming the server this machine
/// now holds.
#[must_use]
pub fn render_remote_updated(saved_server: &SavedServer) -> String {
    render_settled_server_line("updated", saved_server)
}

/// Render a `remote new` or `remote edit` answer the user chose not to save.
#[must_use]
pub fn render_remote_discarded() -> String {
    "nothing was saved.\n".to_string()
}

/// The one line a settled record renders to: `verb`, the name when the record
/// has one, and the address. A record with no pinned fingerprint says when it
/// pins one.
///
/// Example — a named record that was checked renders
/// `saved work at laptop.local:7654.`
fn render_settled_server_line(status_verb: &str, saved_server: &SavedServer) -> String {
    let server_name_prefix = match &saved_server.server_name {
        Some(server_name) => format!("{server_name} at "),
        None => String::new(),
    };
    let certificate_pinning_notice = match saved_server.certificate_fingerprint {
        Some(_) => "",
        None => "; its certificate is pinned on the first connection",
    };
    let server_address = &saved_server.server_address;
    format!("{status_verb} {server_name_prefix}{server_address}{certificate_pinning_notice}.\n")
}

/// One saved server as a listing reports it. The secret is not a field here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RemoteServerRow {
    /// The name the user chose for this server, or `None` when they chose
    /// none.
    #[serde(rename = "name")]
    server_name: Option<String>,
    /// Where the server listens, as `host:port`.
    #[serde(rename = "address")]
    server_address: String,
    /// The sha256 of the certificate this server presented on the first
    /// connection, as 64 lowercase hex characters, or `None` while no
    /// connection to it has opened.
    #[serde(rename = "fingerprint")]
    certificate_fingerprint: Option<String>,
    /// When a connection to this server last opened, or `None` when none has
    /// since it was saved.
    #[serde(rename = "last_used")]
    last_used_at: Option<SystemTime>,
}

/// Column headers for saved-server listings, matching
/// [`render_remote_server_row_cells`].
const REMOTE_HEADERS: &[&str] = &["name", "address", "fingerprint", "last_used"];

/// One [`SavedServer`] as a listing row, leaving its secret behind.
fn build_remote_server_row(saved_server: &SavedServer) -> RemoteServerRow {
    RemoteServerRow {
        server_name: saved_server.server_name.clone(),
        server_address: saved_server.server_address.clone(),
        certificate_fingerprint: saved_server.certificate_fingerprint.clone(),
        last_used_at: saved_server.last_used_at,
    }
}

/// One [`RemoteServerRow`] as table cells, in [`REMOTE_HEADERS`] order.
fn render_remote_server_row_cells(remote_server_row: &RemoteServerRow) -> Vec<String> {
    vec![
        format_optional_cell(remote_server_row.server_name.as_ref()),
        remote_server_row.server_address.clone(),
        format_optional_cell(remote_server_row.certificate_fingerprint.as_ref()),
        format_optional_time_cell(remote_server_row.last_used_at),
    ]
}
