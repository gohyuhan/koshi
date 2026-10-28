//! Rendering for CLI answers: created ids from applied commands, discovery (`list-*`, `inspect`),
//! action introspection (`actions list`, `actions explain`), keymap introspection (the `keys`
//! queries), the `debug` dumps and the `debug events` listing, the two version answers (`version`,
//! `server-version`), the four `share` answers, the three `remote` answers, the `kill-session`
//! ending line, and the `doctor` answer. Read-only queries print as aligned columns
//! (`--format table`, the default) or JSON (`--format json`).
//!
//! List queries render every item as one table row; `inspect`, `actions
//! explain`, and `keys describe` render a single item as `field: value`
//! lines; `debug dump-state` renders one named table per record kind; `debug
//! dump-layout` renders an indented tree, two spaces per level; `debug events`
//! renders one table row per remembered event. `version`
//! prints the one line `--version` prints, and `server-version` renders one
//! table row per koshi server. `share list` renders one table row per grant
//! and `remote list` one per saved server; `share grant`, `share open`,
//! `share revoke`, `remote forget` and `remote set-secret` report one outcome
//! as plain lines and carry no `--format` flag. `kill-session` prints one line on how the
//! session ended, and nothing for a session that quit and left no process
//! running. `doctor` renders one table row per check.
//!
//! Each answer is built once into this module's own rows and reports, and
//! both formats print from them. JSON output serializes them: a JSON array for
//! a list, a JSON object for a single item, and the stable scripting surface.
//! Both formats print an id in the prefixed form a target flag takes
//! (`pane-<uuid>`), a time as RFC 3339 UTC text floored to the whole second
//! (`2025-01-01T00:00:00Z`), and a state as the lowercase word the table
//! prints (`running`, `host`, `not running`). In table cells an absent value
//! prints as `-` and an id list prints as its count; in JSON an absent value
//! is `null`, an id list is the full list, and a size, a rectangle or a cell is
//! an object of numbers.

use std::time::SystemTime;

use koshi_core::action::{
    build_core_action_seeds, ActionHandlerReference, ActionMetadata, ActionReference, ActionScope,
    TargetKind,
};
use koshi_core::discovery::{
    ClientDiscovery, PaneDiscovery, PaneLifecycle, SessionDiscovery, SessionOverview, TabDiscovery,
};
use koshi_core::display_width::compute_text_display_width;
use koshi_core::geometry::{Direction, Size};
use serde::Serialize;

use koshi_core::registry::ActionRegistry;
use koshi_core::resolve::{resolve_action, ResolveError};

use crate::cli::{
    format_argument_verb_invocation, parse_action_reference_text, KeymapScope, OutputFormat,
};
use koshi_link::discovery::{ClientRow, PaneRow, SessionRow, TabRow};

/// The pretty-printed JSON form of `serializable_value`, ending in a newline.
fn render_json<SerializableValue: Serialize>(serializable_value: &SerializableValue) -> String {
    let mut rendered_json_text = serde_json::to_string_pretty(serializable_value)
        .expect("output rows hold only strings, numbers, booleans, and lists and maps of them");
    rendered_json_text.push('\n');
    rendered_json_text
}

/// Aligned columns: a header row, then one row per item, each column padded
/// to its widest cell and separated by two spaces, with no trailing spaces.
///
/// A cell's width is its terminal column count ([`compute_text_display_width`]):
/// `文字文字` is 8 columns wide, `ｶﾞ` (`U+FF76` `U+FF9E`) is 2, and `name` is 4.
fn render_table(column_headers: &[&str], table_rows: Vec<Vec<String>>) -> String {
    let mut column_widths: Vec<usize> = column_headers
        .iter()
        .map(|column_header| compute_text_display_width(column_header))
        .collect();
    for row_cells in &table_rows {
        for (column_width, cell_text) in column_widths.iter_mut().zip(row_cells) {
            *column_width = (*column_width).max(compute_text_display_width(cell_text));
        }
    }
    let mut rendered_output = String::new();
    let header_cells: Vec<String> = column_headers
        .iter()
        .map(|column_header| (*column_header).to_string())
        .collect();
    for row_cells in std::iter::once(&header_cells).chain(table_rows.iter()) {
        let mut rendered_line = String::new();
        for (column_index, (cell_text, column_width)) in
            row_cells.iter().zip(&column_widths).enumerate()
        {
            if column_index > 0 {
                rendered_line.push_str("  ");
            }
            rendered_line.push_str(cell_text);
            let cell_padding_count =
                column_width.saturating_sub(compute_text_display_width(cell_text));
            rendered_line.extend(std::iter::repeat_n(' ', cell_padding_count));
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
