//! Renderers for the three `remote` answers: the listing of every server this
//! machine has saved, the line naming the server a forget dropped, and the
//! line naming the server whose secret was replaced.
//!
//! A saved server's secret is not a field of the listing row, so no format
//! this module renders can print one.

use super::*;
use koshi_ipc::remote_servers::SavedServer;

/// Render a `remote list` answer.
#[must_use]
pub fn render_remote_list(records: &[SavedServer], format: FormatArg) -> String {
    let rows: Vec<RemoteServerRow> = records.iter().map(remote_server_row).collect();
    match format {
        FormatArg::Json => json(&rows),
        FormatArg::Table => table(REMOTE_HEADERS, rows.iter().map(remote_row_cells).collect()),
    }
}

/// Render a `remote forget` answer: the one line naming the server that is no
/// longer saved.
#[must_use]
pub fn render_remote_forget(address: &str) -> String {
    format!("forgot {address}.\n")
}

/// Render a `remote set-secret` answer: the one line naming the server whose
/// secret this machine now presents.
#[must_use]
pub fn render_remote_secret(address: &str) -> String {
    format!("the secret for {address} was replaced.\n")
}

/// One saved server as a listing reports it. The secret is not a field here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RemoteServerRow {
    /// The name the user chose for this server, or `None` when they chose
    /// none.
    name: Option<String>,
    /// Where the server listens, as `host:port`.
    address: String,
    /// The sha256 of the certificate this server presented on the first
    /// connection, as 64 lowercase hex characters.
    fingerprint: String,
    /// When a connection to this server last opened, or `None` when none has
    /// since it was saved.
    last_used: Option<SystemTime>,
}

/// Column headers for saved-server listings, matching [`remote_row_cells`].
const REMOTE_HEADERS: &[&str] = &["name", "address", "fingerprint", "last_used"];

/// One [`SavedServer`] as a listing row, leaving its secret behind.
fn remote_server_row(record: &SavedServer) -> RemoteServerRow {
    RemoteServerRow {
        name: record.name.clone(),
        address: record.address.clone(),
        fingerprint: record.fingerprint.clone(),
        last_used: record.last_used_at,
    }
}

/// One [`RemoteServerRow`] as table cells, in [`REMOTE_HEADERS`] order.
fn remote_row_cells(row: &RemoteServerRow) -> Vec<String> {
    vec![
        row.name.clone().unwrap_or_else(|| "-".to_string()),
        row.address.clone(),
        row.fingerprint.clone(),
        row.last_used.map_or_else(|| "-".to_string(), time_cell),
    ]
}
