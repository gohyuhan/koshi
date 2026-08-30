//! Tests for config domain errors: the version check and its diagnostic,
//! the parse-diagnostic conversion, error messages, and classification.

use super::*;

use std::sync::Arc;

use miette::Diagnostic;

use crate::types::SCHEMA_VERSION;

#[test]
fn current_version_is_accepted() {
    check_version(SCHEMA_VERSION).expect("the current schema version is accepted");
}

#[test]
fn version_zero_is_rejected() {
    let error = check_version(0).expect_err("zero version must fail");
    assert_eq!(
        error.to_string(),
        "config schema version must be at least 1"
    );
}

#[test]
fn newer_version_is_rejected() {
    let err = check_version(SCHEMA_VERSION + 1).expect_err("newer version must fail");
    let ConfigVersionDiagnostic::TooNew { found, supported } = err else {
        panic!("expected newer-version error, got {err:?}");
    };
    assert_eq!(found, SCHEMA_VERSION + 1);
    assert_eq!(supported, SCHEMA_VERSION);
}

#[test]
fn version_diagnostic_message_and_code() {
    let err = check_version(SCHEMA_VERSION + 1).expect_err("newer version must fail");
    assert_eq!(
        err.to_string(),
        format!(
            "config schema version {} is newer than this koshi supports ({})",
            SCHEMA_VERSION + 1,
            SCHEMA_VERSION
        )
    );
    let code = err.code().expect("diagnostic has a code").to_string();
    assert_eq!(code, "koshi::config::version");
}

#[test]
fn too_old_diagnostic_carries_the_version_code() {
    let err = check_version(0).expect_err("zero version must fail");
    let code = err.code().expect("diagnostic has a code").to_string();
    assert_eq!(code, "koshi::config::version");
}

#[test]
fn version_diagnostic_offers_an_upgrade_hint() {
    let err = check_version(SCHEMA_VERSION + 1).expect_err("newer version must fail");
    let help = err.help().expect("diagnostic has a help line").to_string();
    assert_eq!(
        help,
        "upgrade koshi to a build that understands this config"
    );
}

#[test]
fn parse_error_shows_path_and_detail() {
    let err = ConfigError::Parse {
        path: "koshi.kdl".to_string(),
        detail: "unexpected token".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "config parse error in koshi.kdl: unexpected token"
    );
}

#[test]
fn validation_error_quotes_the_key() {
    let err = ConfigError::Validation {
        key: "scrollback".to_string(),
        detail: "must be a positive integer".to_string(),
    };
    assert_eq!(
        err.to_string(),
        "invalid config key `scrollback`: must be a positive integer"
    );
}

#[test]
fn validation_builds_the_named_key_and_detail() {
    let err = validation("scrollback", "must be a positive integer");
    let ConfigError::Validation { key, detail } = err else {
        panic!("expected ConfigError::Validation, got {err:?}");
    };
    assert_eq!(key, "scrollback");
    assert_eq!(detail, "must be a positive integer");
}

#[test]
fn parse_conversion_without_sub_diagnostics_uses_the_kdl_display() {
    let raw = KdlError {
        input: Arc::new(String::new()),
        diagnostics: Vec::new(),
    };
    let diag = ConfigParseDiagnostic::new(Path::new("koshi.kdl"), raw);
    let ConfigError::Parse { path, detail } = ConfigError::from(diag) else {
        panic!("expected ConfigError::Parse");
    };
    assert_eq!(path, "koshi.kdl");
    assert_eq!(detail, "Failed to parse KDL document");
}

#[test]
fn config_errors_classify_as_recoverable_config_problems() {
    let err = ConfigError::Validation {
        key: "scrollback".to_string(),
        detail: "x".to_string(),
    };
    assert_eq!(err.category(), DomainCategory::Config);
    assert_eq!(err.severity(), Severity::Recoverable);
}

#[test]
fn color_bad_length_reports_the_digit_count() {
    let err = ColorParseError::BadLength { got: 5 };
    assert_eq!(
        err.to_string(),
        "color must be 6 hex digits (#RRGGBB), got 5"
    );
}

#[test]
fn color_bad_digit_quotes_the_offending_value() {
    let err = ColorParseError::BadDigit {
        value: "#gg0011".to_string(),
    };
    assert_eq!(err.to_string(), "color `#gg0011` contains a non-hex digit");
}

#[test]
fn color_parse_errors_compare_by_value() {
    assert_eq!(
        ColorParseError::BadLength { got: 5 },
        ColorParseError::BadLength { got: 5 }
    );
    assert_ne!(
        ColorParseError::BadLength { got: 5 },
        ColorParseError::BadLength { got: 4 }
    );
    assert_ne!(
        ColorParseError::BadLength { got: 6 },
        ColorParseError::BadDigit {
            value: "z".to_string()
        }
    );
}
