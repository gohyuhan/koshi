//! Rendering for CLI answers: created ids from applied commands, discovery
//! (`list-*`, `inspect`), action introspection (`actions list`, `actions
//! explain`), and keymap introspection (the `keys` queries). Read-only
//! queries print as aligned columns (`--format table`, the default) or JSON
//! (`--format json`).
//!
//! List queries render every item as one table row; `inspect`, `actions
//! explain`, and `keys describe` render a single item as `field: value`
//! lines. JSON output is the serde form of the rendered structs — the
//! [`crate::discovery`] listing rows, the [`koshi_core::discovery`] records
//! an `inspect` reports, and this module's own summary/detail structs — a
//! JSON array for a list, a JSON object for a single item, and the stable
//! scripting surface. In table cells an absent value prints as `-`, an id
//! list prints as its count (full ids are in the JSON form), and a timestamp
//! prints as whole seconds since the Unix epoch.

use std::time::SystemTime;

use koshi_core::action::{
    core_action_seeds, ActionHandlerRef, ActionMetadata, ActionRef, ActionScope, ActionStatus,
    TargetKind,
};
use koshi_core::discovery::{ClientInfo, PaneInfo, PaneState, SessionInfo, TabInfo};
use koshi_core::geometry::Size;
use serde::Serialize;

use crate::cli::{FormatArg, ScopeArg};
use crate::discovery::{ClientRow, PaneRow, SessionRow, TabRow};

/// The pretty-printed JSON form of `value`, ending in a newline.
fn json<T: Serialize>(value: &T) -> String {
    let mut rendered = serde_json::to_string_pretty(value).expect(
        "output structs serialize: strings are valid, paths render lossily, clocks post-epoch",
    );
    rendered.push('\n');
    rendered
}

/// Aligned columns: a header row, then one row per item, each column padded
/// to its widest cell and separated by two spaces, with no trailing spaces.
fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    // Each column's width is the widest cell in that column, starting from
    // the header's own width.
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
    // Render the header first, then every data row, using the same padding logic.
    for row in std::iter::once(&header_cells).chain(rows.iter()) {
        let mut line = String::new();
        for (index, (cell, width)) in row.iter().zip(&widths).enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            let padding = width.saturating_sub(cell.chars().count());
            // Pad every cell except the last, whose trailing spaces get
            // trimmed off the line below anyway.
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

mod actions;
mod command;
mod entities;
mod keys;

pub use actions::*;
pub use command::*;
pub use entities::*;
pub use keys::*;

#[cfg(test)]
mod tests;
