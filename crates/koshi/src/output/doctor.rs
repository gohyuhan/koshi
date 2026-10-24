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
    let doctor_rows: Vec<DoctorRow> = check_rows.iter().map(build_doctor_row).collect();
    match output_format {
        OutputFormat::Json => render_json(&doctor_rows),
        OutputFormat::Table => render_table(
            DOCTOR_HEADERS,
            doctor_rows.iter().map(format_doctor_row_cells).collect(),
        ),
    }
}

/// One check as both formats of `doctor` print it.
#[derive(Serialize)]
struct DoctorRow {
    /// The check's name.
    check_name: &'static str,
    /// What the check concluded, as [`format_verdict_cell`] names it.
    verdict: &'static str,
    /// The fact behind the verdict, on one line.
    reason: String,
    /// What to do about it, or `None` when there is nothing to do.
    help: Option<String>,
    /// The full text behind a shortened `reason`, or `None` when `reason`
    /// already says the whole thing. The table leaves it out.
    detail: Option<String>,
}

/// One [`DoctorCheckRow`] as both formats print it.
fn build_doctor_row(check_row: &DoctorCheckRow) -> DoctorRow {
    DoctorRow {
        check_name: check_row.check_name,
        verdict: format_verdict_cell(check_row.outcome.verdict),
        reason: check_row.outcome.reason.clone(),
        help: check_row.outcome.help.clone(),
        detail: check_row.outcome.detail.clone(),
    }
}

/// Column headers for doctor answers, matching [`format_doctor_row_cells`].
const DOCTOR_HEADERS: &[&str] = &["check", "verdict", "reason", "help"];

/// One [`DoctorRow`] as table cells, in [`DOCTOR_HEADERS`] order. A row with
/// no help prints `-` in that column.
fn format_doctor_row_cells(doctor_row: &DoctorRow) -> Vec<String> {
    vec![
        doctor_row.check_name.to_string(),
        doctor_row.verdict.to_string(),
        doctor_row.reason.clone(),
        format_optional_cell(doctor_row.help.as_ref()),
    ]
}

/// The verdict word: `"ok"`, `"warn"` or `"fail"`.
fn format_verdict_cell(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Ok => "ok",
        Verdict::Warn => "warn",
        Verdict::Fail => "fail",
    }
}

#[cfg(test)]
mod tests;
