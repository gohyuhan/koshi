//! Tests for config domain errors: the version check and its diagnostic,
//! the parse-diagnostic conversion, and error messages.

use super::*;

use std::sync::Arc;

use miette::Diagnostic;

use crate::types::SCHEMA_VERSION;

#[test]
fn current_version_is_accepted() {
    validate_config_schema_version(SCHEMA_VERSION, SCHEMA_VERSION)
        .expect("the current schema version is accepted");
}

#[test]
fn version_zero_is_rejected() {
    let version_error =
        validate_config_schema_version(0, SCHEMA_VERSION).expect_err("zero version must fail");
    assert_eq!(
        version_error.to_string(),
        "config schema version must be at least 1"
    );
}

#[test]
fn newer_version_is_rejected() {
    let version_error = validate_config_schema_version(SCHEMA_VERSION + 1, SCHEMA_VERSION)
        .expect_err("newer version must fail");
    let ConfigVersionDiagnostic::TooNew {
        declared_schema_version,
        supported_schema_version,
    } = version_error
    else {
        panic!("expected newer-version error, got {version_error:?}");
    };
    assert_eq!(declared_schema_version, SCHEMA_VERSION + 1);
    assert_eq!(supported_schema_version, SCHEMA_VERSION);
}

#[test]
fn version_diagnostic_message_and_code() {
    let version_error = validate_config_schema_version(SCHEMA_VERSION + 1, SCHEMA_VERSION)
        .expect_err("newer version must fail");
    assert_eq!(
        version_error.to_string(),
        format!(
            "config schema version {} is newer than this koshi supports ({})",
            SCHEMA_VERSION + 1,
            SCHEMA_VERSION
        )
    );
    let diagnostic_code = version_error
        .code()
        .expect("diagnostic has a code")
        .to_string();
    assert_eq!(diagnostic_code, "koshi::config::version");
}

#[test]
fn too_old_diagnostic_carries_the_version_code() {
    let version_error =
        validate_config_schema_version(0, SCHEMA_VERSION).expect_err("zero version must fail");
    let diagnostic_code = version_error
        .code()
        .expect("diagnostic has a code")
        .to_string();
    assert_eq!(diagnostic_code, "koshi::config::version");
}

#[test]
fn version_diagnostic_offers_an_upgrade_hint() {
    let version_error = validate_config_schema_version(SCHEMA_VERSION + 1, SCHEMA_VERSION)
        .expect_err("newer version must fail");
    let upgrade_help = version_error
        .help()
        .expect("diagnostic has a help line")
        .to_string();
    assert_eq!(
        upgrade_help,
        "upgrade koshi to a build that understands this config"
    );
}

#[test]
fn a_parse_error_reads_as_its_detail_and_names_no_file() {
    let parse_error = ConfigError::Parse {
        parse_error_detail: "line 1, column 6: unexpected token".to_string(),
    };
    assert_eq!(
        parse_error.to_string(),
        "line 1, column 6: unexpected token"
    );
}

#[test]
fn a_validation_error_reads_as_every_problem_joined_and_lists_each_one() {
    let validation_error = ConfigError::Validation {
        problem_messages: vec![
            "line 2, column 1: ignored unknown key `frobnicate`; did you mean `update`?"
                .to_string(),
            "line 5, column 10: invalid config key `update.auto-check`: expected a boolean \
             (#true or #false)"
                .to_string(),
        ],
    };
    assert_eq!(
        validation_error.to_string(),
        "line 2, column 1: ignored unknown key `frobnicate`; did you mean `update`?; line 5, \
         column 10: invalid config key `update.auto-check`: expected a boolean (#true or #false)"
    );
    assert_eq!(
        validation_error.list_problem_messages(),
        [
            "line 2, column 1: ignored unknown key `frobnicate`; did you mean `update`?",
            "line 5, column 10: invalid config key `update.auto-check`: expected a boolean \
             (#true or #false)",
        ]
    );
}

#[test]
fn a_parse_error_lists_its_one_detail() {
    let parse_error = ConfigError::Parse {
        parse_error_detail: "line 1, column 6: unexpected token".to_string(),
    };
    assert_eq!(
        parse_error.list_problem_messages(),
        ["line 1, column 6: unexpected token"]
    );
}

#[test]
fn format_problem_at_line_and_column_names_where_the_span_starts() {
    let config_source_text = "version 2\nupdate {\n    auto-check \"yes\"\n}";
    let auto_check_byte_offset = config_source_text
        .find("auto-check")
        .expect("the text holds the node");
    assert_eq!(
        format_problem_at_line_and_column(
            config_source_text,
            SourceSpan::from((auto_check_byte_offset, "auto-check \"yes\"".len())),
            "bad value",
        ),
        "line 3, column 5: bad value"
    );
}

#[test]
fn a_version_newer_than_the_supported_schema_given_is_rejected() {
    assert_eq!(
        validate_config_schema_version(3, 2)
            .expect_err("3 is newer than 2")
            .to_string(),
        "config schema version 3 is newer than this koshi supports (2)"
    );
    validate_config_schema_version(3, 3).expect("3 is supported when 3 is the newest");
}

#[test]
fn parse_conversion_without_sub_diagnostics_uses_the_kdl_display() {
    let raw_kdl_error = KdlError {
        input: Arc::new(String::new()),
        diagnostics: Vec::new(),
    };
    let parse_diagnostic =
        ConfigParseDiagnostic::from_kdl_error(Path::new("koshi.kdl"), raw_kdl_error);
    let ConfigError::Parse { parse_error_detail } = ConfigError::from(parse_diagnostic) else {
        panic!("expected ConfigError::Parse");
    };
    assert_eq!(parse_error_detail, "Failed to parse KDL document");
}

#[test]
fn parse_conversion_names_the_line_and_column_of_the_first_sub_diagnostic() {
    // `é` is two bytes and one column: the unclosed `{` starts at byte 12 and
    // sits at column 10 of line 2.
    let parse_diagnostic = crate::parser::parse_kdl(Path::new("koshi.kdl"), "a\nnode \"é\" {")
        .expect_err("the block is not closed");
    assert_eq!(
        parse_diagnostic.build_config_error().to_string(),
        "line 2, column 10: No closing '}' for child block"
    );
}

#[test]
fn line_and_column_count_every_line_break_and_characters_not_bytes() {
    // (source text, byte offset, expected located message)
    let line_and_column_cases = [
        ("pane {", 0, "line 1, column 1: problem"),
        ("pane {", 5, "line 1, column 6: problem"),
        ("a\nb", 2, "line 2, column 1: problem"),
        ("a\r\nb", 3, "line 2, column 1: problem"),
        ("a\rb", 2, "line 2, column 1: problem"),
        ("a\n\nb", 3, "line 3, column 1: problem"),
        ("éé {", 5, "line 1, column 4: problem"),
        ("ab", 2, "line 1, column 3: problem"),
        ("ab", 9, "line 1, column 3: problem"),
    ];
    for (source_text, byte_offset, expected_located_message) in line_and_column_cases {
        assert_eq!(
            format_problems_at_line_and_column(source_text, [(byte_offset, "problem")]),
            [expected_located_message],
            "byte {byte_offset} of {source_text:?}"
        );
    }
}

#[test]
fn ascending_problems_each_get_their_own_location_in_one_pass() {
    // Byte 2 is the `\n` of `\r\n`: the problem there and the one at byte 3
    // both sit at line 2, column 1.
    assert_eq!(
        format_problems_at_line_and_column(
            "a\r\nb\nc",
            [
                (2, "at the line feed"),
                (3, "at b"),
                (3, "at b again"),
                (5, "at c"),
                (9, "past the end"),
            ]
        ),
        [
            "line 2, column 1: at the line feed",
            "line 2, column 1: at b",
            "line 2, column 1: at b again",
            "line 3, column 1: at c",
            "line 3, column 2: past the end",
        ]
    );
}

#[test]
fn color_bad_length_reports_the_digit_count() {
    let color_error = ColorParseError::BadLength { character_count: 5 };
    assert_eq!(
        color_error.to_string(),
        "color must be 6 hex digits (#RRGGBB), got 5"
    );
}

#[test]
fn color_bad_digit_quotes_the_offending_value() {
    let color_error = ColorParseError::BadDigit {
        invalid_hex_text: "#gg0011".to_string(),
    };
    assert_eq!(
        color_error.to_string(),
        "color `#gg0011` contains a non-hex digit"
    );
}
