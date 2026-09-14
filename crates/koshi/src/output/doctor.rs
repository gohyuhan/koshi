//! Renderer for the `doctor` answer: one row per check, with the verdict, the
//! fact behind it, and what to do about it.
//!
//! The table prints four columns and leaves out each row's full text.
//! `--format json` prints every field, that full text included.

use super::*;
use crate::doctor::{DoctorCheckRow, Verdict};

/// Render a `doctor` answer.
#[must_use]
pub fn render_doctor(check_rows: &[DoctorCheckRow], output_format: OutputFormat) -> String {
    match output_format {
        OutputFormat::Json => render_json(&check_rows),
        OutputFormat::Table => render_table(
            DOCTOR_HEADERS,
            check_rows.iter().map(doctor_row_cells).collect(),
        ),
    }
}

/// Column headers for doctor answers, matching [`doctor_row_cells`].
const DOCTOR_HEADERS: &[&str] = &["check", "verdict", "reason", "help"];

/// One [`DoctorCheckRow`] as table cells, in [`DOCTOR_HEADERS`] order. A row with
/// no help prints `-` in that column.
fn doctor_row_cells(check_row: &DoctorCheckRow) -> Vec<String> {
    vec![
        check_row.check_name.to_string(),
        verdict_cell(check_row.outcome.verdict).to_string(),
        check_row.outcome.reason.clone(),
        format_optional_cell(check_row.outcome.help.as_ref()),
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
