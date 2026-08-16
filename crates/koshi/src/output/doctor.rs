//! Renderer for the `doctor` answer: one row per check, with the verdict, the
//! fact behind it, and what to do about it.
//!
//! The table prints four columns and leaves out each row's full text.
//! `--format json` prints every field, that full text included.

use super::*;
use crate::doctor::{CheckRow, Verdict};

/// Render a `doctor` answer.
#[must_use]
pub fn render_doctor(rows: &[CheckRow], format: FormatArg) -> String {
    match format {
        FormatArg::Json => json(&rows),
        FormatArg::Table => table(DOCTOR_HEADERS, rows.iter().map(doctor_row_cells).collect()),
    }
}

/// Column headers for doctor answers, matching [`doctor_row_cells`].
const DOCTOR_HEADERS: &[&str] = &["check", "verdict", "reason", "help"];

/// One [`CheckRow`] as table cells, in [`DOCTOR_HEADERS`] order. A row with
/// no help prints `-` in that column.
fn doctor_row_cells(row: &CheckRow) -> Vec<String> {
    vec![
        row.name.to_string(),
        verdict_cell(row.verdict).to_string(),
        row.reason.clone(),
        row.help.clone().unwrap_or_else(|| "-".to_string()),
    ]
}

/// The verdict cell: `"ok"`, `"warn"` or `"fail"`.
fn verdict_cell(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Ok => "ok",
        Verdict::Warn => "warn",
        Verdict::Fail => "fail",
    }
}

#[cfg(test)]
mod tests;
