//! Rendering for CLI answers: created ids from applied commands, discovery
//! (`list-*`, `inspect`), action introspection (`actions list`, `actions
//! explain`), keymap introspection (the `keys` queries), the `debug`
//! dumps and the `debug events` listing, the two version answers (`version`, `server-version`), the three
//! `share` answers, the three `remote` answers, and the `doctor` answer.
//! Read-only queries print as aligned columns (`--format table`, the default)
//! or JSON (`--format json`).
//!
//! List queries render every item as one table row; `inspect`, `actions
//! explain`, and `keys describe` render a single item as `field: value`
//! lines; `debug dump-state` renders one named table per record kind; `debug
//! dump-layout` renders an indented tree, two spaces per level; `debug events`
//! renders one table row per remembered event. `version`
//! prints the one line `--version` prints, and `server-version` renders one
//! table row per koshi server. `share list` renders one table row per grant
//! and `remote list` one per saved server; `share grant`, `share revoke`,
//! `remote forget` and `remote set-secret` report one outcome as plain lines
//! and carry no `--format` flag. `doctor` renders one table row per check.
//! JSON output is
//! the serde form of the rendered structs — the [`koshi_link::discovery`] listing
//! rows, the [`koshi_core::discovery`] records an `inspect` reports, and this
//! module's own summary/detail structs — a JSON array for a list, a JSON
//! object for a single item, and the stable scripting surface. In table cells
//! an absent value prints as `-`, an id list prints as its count (full ids are
//! in the JSON form), and a timestamp prints as whole seconds since the Unix
//! epoch.

use std::time::SystemTime;

use koshi_core::action::{
    build_core_action_seeds, ActionHandlerReference, ActionMetadata, ActionReference, ActionScope,
    ActionStatus, TargetKind,
};
use koshi_core::discovery::{
    ClientDiscovery, PaneDiscovery, PaneLifecycle, SessionDiscovery, SessionOverview, TabDiscovery,
};
use koshi_core::geometry::Size;
use serde::Serialize;

use crate::cli::{KeymapScope, OutputFormat};
use koshi_link::discovery::{ClientRow, PaneRow, SessionRow, TabRow};

/// The pretty-printed JSON form of `serializable_value`, ending in a newline.
fn render_json<SerializableValue: Serialize>(serializable_value: &SerializableValue) -> String {
    let mut rendered = serde_json::to_string_pretty(serializable_value).expect(
        "output structs serialize: strings are valid, paths render lossily, clocks post-epoch",
    );
    rendered.push('\n');
    rendered
}

/// Aligned columns: a header row, then one row per item, each column padded
/// to its widest cell and separated by two spaces, with no trailing spaces.
fn render_table(column_headers: &[&str], table_rows: Vec<Vec<String>>) -> String {
    // Each column's width is the widest cell in that column, starting from
    // the header's own width.
    let mut column_widths: Vec<usize> = column_headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row_cells in &table_rows {
        for (column_width, cell_text) in column_widths.iter_mut().zip(row_cells) {
            *column_width = (*column_width).max(cell_text.chars().count());
        }
    }
    let mut rendered_output = String::new();
    let header_cells: Vec<String> = column_headers
        .iter()
        .map(|header| (*header).to_string())
        .collect();
    // Render the header first, then every data row, using the same padding logic.
    for row_cells in std::iter::once(&header_cells).chain(table_rows.iter()) {
        let mut rendered_line = String::new();
        for (column_index, (cell_text, column_width)) in
            row_cells.iter().zip(&column_widths).enumerate()
        {
            if column_index > 0 {
                rendered_line.push_str("  ");
            }
            rendered_line.push_str(cell_text);
            let cell_padding_count = column_width.saturating_sub(cell_text.chars().count());
            // Pad every cell except the last, whose trailing spaces get
            // trimmed off the line below anyway.
            if column_index < row_cells.len() - 1 {
                rendered_line.extend(std::iter::repeat_n(' ', cell_padding_count));
            }
        }
        rendered_output.push_str(rendered_line.trim_end());
        rendered_output.push('\n');
    }
    rendered_output
}

/// A single item as `field: value` lines, one per header.
fn render_fields(field_headers: &[&str], field_values: Vec<String>) -> String {
    let mut rendered_output = String::new();
    for (field_header, field_value) in field_headers.iter().zip(field_values) {
        rendered_output.push_str(field_header);
        rendered_output.push_str(": ");
        rendered_output.push_str(&field_value);
        rendered_output.push('\n');
    }
    rendered_output
}

mod actions;
mod command;
mod doctor;
mod entities;
mod events;
mod keys;
mod layout;
mod remote;
mod share;
mod version;

pub use actions::*;
pub use command::*;
pub use doctor::*;
pub use entities::*;
pub use events::*;
pub use keys::*;
pub use layout::*;
pub use remote::*;
pub use share::*;
pub use version::*;

#[cfg(test)]
mod tests;
