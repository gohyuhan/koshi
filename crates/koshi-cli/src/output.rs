//! Rendering for discovery query answers: each `*Info` kind prints as
//! aligned columns (`--format table`, the default) or JSON (`--format
//! json`).
//!
//! List queries render every item as one table row; `inspect` renders a
//! single item as `field: value` lines. JSON output is the serde form of
//! the [`koshi_core::discovery`] structs — a JSON array for a list, a JSON
//! object for a single item — and is the stable scripting surface. In table
//! cells an absent value prints as `-`, an id list prints as its count
//! (full ids are in the JSON form), and a timestamp prints as whole seconds
//! since the Unix epoch.

use std::time::SystemTime;

use koshi_core::discovery::{ClientInfo, PaneInfo, PaneState, SessionInfo, TabInfo};
use koshi_core::geometry::{Rect, Size};
use serde::Serialize;

use crate::cli::FormatArg;

/// Render a `list-sessions` answer.
#[must_use]
pub fn render_sessions(sessions: &[SessionInfo], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&sessions),
        FormatArg::Table => table(SESSION_HEADERS, sessions.iter().map(session_row).collect()),
    }
}

/// Render an `inspect session` answer.
#[must_use]
pub fn render_session(session: &SessionInfo, format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(session),
        FormatArg::Table => fields(SESSION_HEADERS, session_row(session)),
    }
}

/// Render a `list-tabs` answer.
#[must_use]
pub fn render_tabs(tabs: &[TabInfo], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&tabs),
        FormatArg::Table => table(TAB_HEADERS, tabs.iter().map(tab_row).collect()),
    }
}

/// Render an `inspect tab` answer.
#[must_use]
pub fn render_tab(tab: &TabInfo, format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(tab),
        FormatArg::Table => fields(TAB_HEADERS, tab_row(tab)),
    }
}

/// Render a `list-panes` answer.
#[must_use]
pub fn render_panes(panes: &[PaneInfo], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&panes),
        FormatArg::Table => table(PANE_HEADERS, panes.iter().map(pane_row).collect()),
    }
}

/// Render an `inspect pane` answer.
#[must_use]
pub fn render_pane(pane: &PaneInfo, format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(pane),
        FormatArg::Table => fields(PANE_HEADERS, pane_row(pane)),
    }
}

/// Render a `list-clients` answer.
#[must_use]
pub fn render_clients(clients: &[ClientInfo], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&clients),
        FormatArg::Table => table(CLIENT_HEADERS, clients.iter().map(client_row).collect()),
    }
}

/// Render an `inspect client` answer.
#[must_use]
pub fn render_client(client: &ClientInfo, format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(client),
        FormatArg::Table => fields(CLIENT_HEADERS, client_row(client)),
    }
}

/// Column headers for [`SessionInfo`] rows, matching [`session_row`] order.
const SESSION_HEADERS: &[&str] = &["id", "name", "created_at", "clients", "panes"];

/// Column headers for [`TabInfo`] rows, matching [`tab_row`] order.
const TAB_HEADERS: &[&str] = &["id", "name", "index", "active_pane", "panes"];

/// Column headers for [`PaneInfo`] rows, matching [`pane_row`] order.
const PANE_HEADERS: &[&str] = &[
    "id",
    "tab",
    "session",
    "title",
    "cwd",
    "command",
    "state",
    "focused_by",
    "rect",
];

/// Column headers for [`ClientInfo`] rows, matching [`client_row`] order.
const CLIENT_HEADERS: &[&str] = &[
    "id",
    "session",
    "attached_at",
    "viewport",
    "active_tab",
    "focused_pane",
    "lock",
];

/// One [`SessionInfo`] as table cells, in [`SESSION_HEADERS`] order.
fn session_row(session: &SessionInfo) -> Vec<String> {
    vec![
        session.id.to_string(),
        session.name.clone(),
        time_cell(session.created_at),
        session.attached_clients.len().to_string(),
        session.pane_count.to_string(),
    ]
}

/// One [`TabInfo`] as table cells, in [`TAB_HEADERS`] order.
fn tab_row(tab: &TabInfo) -> Vec<String> {
    vec![
        tab.id.to_string(),
        tab.name.clone(),
        tab.index.to_string(),
        opt_cell(tab.active_pane.as_ref()),
        tab.pane_count.to_string(),
    ]
}

/// One [`PaneInfo`] as table cells, in [`PANE_HEADERS`] order.
fn pane_row(pane: &PaneInfo) -> Vec<String> {
    vec![
        pane.id.to_string(),
        pane.tab_id.to_string(),
        pane.session_id.to_string(),
        opt_cell(pane.title.as_ref()),
        match &pane.cwd {
            Some(cwd) => cwd.display().to_string(),
            None => "-".to_string(),
        },
        match &pane.command {
            Some(argv) => argv.join(" "),
            None => "-".to_string(),
        },
        state_cell(pane.state),
        pane.focused_by_clients.len().to_string(),
        match pane.layout_rect {
            Some(rect) => rect_cell(rect),
            None => "-".to_string(),
        },
    ]
}

/// One [`ClientInfo`] as table cells, in [`CLIENT_HEADERS`] order.
fn client_row(client: &ClientInfo) -> Vec<String> {
    vec![
        client.id.to_string(),
        client.session_id.to_string(),
        time_cell(client.attached_at),
        size_cell(client.viewport_size),
        client.active_tab.to_string(),
        opt_cell(client.focused_pane.as_ref()),
        format!("{:?}", client.lock_state),
    ]
}

/// An optional value as a cell: its display form, or `-` when absent.
fn opt_cell<T: std::fmt::Display>(value: Option<&T>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "-".to_string(),
    }
}

/// A timestamp as a cell: whole seconds since the Unix epoch.
fn time_cell(time: SystemTime) -> String {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs().to_string(),
        Err(_) => "-".to_string(),
    }
}

/// A size as a cell: `<cols>x<rows>`.
fn size_cell(size: Size) -> String {
    format!("{}x{}", size.cols, size.rows)
}

/// A rectangle as a cell: `<cols>x<rows>@<x>,<y>`.
fn rect_cell(rect: Rect) -> String {
    format!(
        "{}x{}@{},{}",
        rect.size.cols, rect.size.rows, rect.origin.x, rect.origin.y
    )
}

/// A pane state as a cell: its lowercase name, with the exit code appended
/// as `exited(<code>)` when one was observed and `exited(-)` when not.
fn state_cell(state: PaneState) -> String {
    match state {
        PaneState::Spawning => "spawning".to_string(),
        PaneState::Running => "running".to_string(),
        PaneState::Exited { code: Some(code) } => format!("exited({code})"),
        PaneState::Exited { code: None } => "exited(-)".to_string(),
        PaneState::Closing => "closing".to_string(),
    }
}

/// The pretty-printed JSON form of `value`, ending in a newline.
fn json<T: Serialize>(value: &T) -> String {
    let mut rendered = serde_json::to_string_pretty(value)
        .expect("discovery structs serialize: paths render lossily and clocks are post-epoch");
    rendered.push('\n');
    rendered
}

/// Aligned columns: a header row, then one row per item, each column padded
/// to its widest cell and separated by two spaces, with no trailing spaces.
fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut rendered = String::new();
    let header_cells: Vec<String> = headers.iter().map(|header| (*header).to_string()).collect();
    for row in std::iter::once(&header_cells).chain(rows.iter()) {
        let mut line = String::new();
        for (index, (cell, width)) in row.iter().zip(&widths).enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            let padding = width.saturating_sub(cell.chars().count());
            if index < row.len() - 1 {
                line.extend(std::iter::repeat_n(' ', padding));
            }
        }
        rendered.push_str(line.trim_end());
        rendered.push('\n');
    }
    rendered
}

/// A single item as `field: value` lines, one per header.
fn fields(headers: &[&str], row: Vec<String>) -> String {
    let mut rendered = String::new();
    for (header, cell) in headers.iter().zip(row) {
        rendered.push_str(header);
        rendered.push_str(": ");
        rendered.push_str(&cell);
        rendered.push('\n');
    }
    rendered
}

#[cfg(test)]
mod tests;
