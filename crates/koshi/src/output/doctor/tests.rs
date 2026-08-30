//! Tests for the doctor renderers: the aligned table and the JSON array.

use super::*;
use crate::doctor::Outcome;

/// One row named `name`, carrying `verdict`, `reason`, `help` and `detail`.
fn row(
    name: &'static str,
    verdict: Verdict,
    reason: &str,
    help: Option<&str>,
    detail: Option<&str>,
) -> CheckRow {
    CheckRow {
        name,
        outcome: Outcome {
            verdict,
            reason: reason.to_string(),
            help: help.map(str::to_string),
            detail: detail.map(str::to_string),
        },
    }
}

/// Two rows: an ok row with no help, and a warn row with help.
fn sample() -> Vec<CheckRow> {
    vec![
        row("config", Verdict::Ok, "1 config file validated", None, None),
        row(
            "terminal",
            Verdict::Warn,
            "TERM is not set",
            Some("set TERM before running koshi"),
            None,
        ),
    ]
}

#[test]
fn the_table_pads_every_column_and_prints_a_dash_for_a_row_with_no_help() {
    let rendered = render_doctor(&sample(), FormatArg::Table);

    assert_eq!(
        rendered,
        "check     verdict  reason                   help\n\
         config    ok       1 config file validated  -\n\
         terminal  warn     TERM is not set          set TERM before running koshi\n"
    );
}

#[test]
fn the_json_form_is_one_object_per_row() {
    let rendered = render_doctor(&sample(), FormatArg::Json);

    assert_eq!(
        rendered,
        "[\n  \
           {\n    \
             \"name\": \"config\",\n    \
             \"verdict\": \"ok\",\n    \
             \"reason\": \"1 config file validated\",\n    \
             \"help\": null,\n    \
             \"detail\": null\n  \
           },\n  \
           {\n    \
             \"name\": \"terminal\",\n    \
             \"verdict\": \"warn\",\n    \
             \"reason\": \"TERM is not set\",\n    \
             \"help\": \"set TERM before running koshi\",\n    \
             \"detail\": null\n  \
           }\n\
         ]\n"
    );
}

#[test]
fn no_rows_render_the_header_alone_and_an_empty_json_array() {
    assert_eq!(
        render_doctor(&[], FormatArg::Table),
        "check  verdict  reason  help\n"
    );
    assert_eq!(render_doctor(&[], FormatArg::Json), "[]\n");
}

/// One row whose `reason` is short and whose `detail` holds the whole text.
fn shortened() -> Vec<CheckRow> {
    vec![row(
        "router",
        Verdict::Fail,
        "a router is listening and did not answer",
        Some("end every koshi process on this machine and start one again"),
        Some(
            "this router has no request kind named RemoteStatus, and the running router is an \
             older koshi that does not report its build",
        ),
    )]
}

#[test]
fn the_table_leaves_the_full_text_out() {
    let rendered = render_doctor(&shortened(), FormatArg::Table);

    assert_eq!(
        rendered,
        "check   verdict  reason                                    help\n\
         router  fail     a router is listening and did not answer  end every koshi process on this machine and start one again\n"
    );
}

#[test]
fn the_json_form_carries_the_full_text() {
    let rendered = render_doctor(&shortened(), FormatArg::Json);

    assert_eq!(
        rendered,
        "[\n  \
           {\n    \
             \"name\": \"router\",\n    \
             \"verdict\": \"fail\",\n    \
             \"reason\": \"a router is listening and did not answer\",\n    \
             \"help\": \"end every koshi process on this machine and start one again\",\n    \
             \"detail\": \"this router has no request kind named RemoteStatus, and the running router is an older koshi that does not report its build\"\n  \
           }\n\
         ]\n"
    );
}

#[test]
fn a_failed_row_renders_the_fail_verdict() {
    let rows = vec![row(
        "shell",
        Verdict::Fail,
        "a new pane would run /bin/nope, which is not on this machine",
        Some("set SHELL to a shell that exists"),
        None,
    )];

    let rendered = render_doctor(&rows, FormatArg::Table);

    assert_eq!(
        rendered,
        "check  verdict  reason                                                        help\n\
         shell  fail     a new pane would run /bin/nope, which is not on this machine  set SHELL to a shell that exists\n"
    );
}
