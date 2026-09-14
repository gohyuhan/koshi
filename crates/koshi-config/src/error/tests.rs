//! Tests for config domain errors: the version check and its diagnostic,
//! the parse-diagnostic conversion, error messages, and classification.

use super::*;

use std::sync::Arc;

use miette::Diagnostic;

use crate::types::SCHEMA_VERSION;

#[test]
fn current_version_is_accepted() {
    validate_config_schema_version(SCHEMA_VERSION).expect("the current schema version is accepted");
}

#[test]
fn version_zero_is_rejected() {
    let version_error = validate_config_schema_version(0).expect_err("zero version must fail");
    assert_eq!(
        version_error.to_string(),
        "config schema version must be at least 1"
    );
}

#[test]
fn newer_version_is_rejected() {
    let version_error =
        validate_config_schema_version(SCHEMA_VERSION + 1).expect_err("newer version must fail");
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
    let version_error =
        validate_config_schema_version(SCHEMA_VERSION + 1).expect_err("newer version must fail");
    assert_eq!(
        version_error.to_string(),
        format!(
            "config schema version {} is newer than this koshi supports ({})",
            SCHEMA_VERSION + 1,
            SCHEMA_VERSION
        )
    );
    let code = version_error
        .code()
        .expect("diagnostic has a code")
        .to_string();
    assert_eq!(code, "koshi::config::version");
}

#[test]
fn too_old_diagnostic_carries_the_version_code() {
    let version_error = validate_config_schema_version(0).expect_err("zero version must fail");
    let code = version_error
        .code()
        .expect("diagnostic has a code")
        .to_string();
    assert_eq!(code, "koshi::config::version");
}

#[test]
fn version_diagnostic_offers_an_upgrade_hint() {
    let version_error =
        validate_config_schema_version(SCHEMA_VERSION + 1).expect_err("newer version must fail");
    let help = version_error
        .help()
        .expect("diagnostic has a help line")
        .to_string();
    assert_eq!(
        help,
        "upgrade koshi to a build that understands this config"
    );
}

#[test]
fn parse_error_shows_path_and_detail() {
    let parse_error = ConfigError::Parse {
        config_path: "koshi.kdl".to_string(),
        parse_error_detail: "unexpected token".to_string(),
    };
    assert_eq!(
        parse_error.to_string(),
        "config parse error in koshi.kdl: unexpected token"
    );
}

#[test]
fn validation_error_quotes_the_key() {
    let validation_error = ConfigError::Validation {
        config_key: "scrollback".to_string(),
        validation_detail: "must be a positive integer".to_string(),
    };
    assert_eq!(
        validation_error.to_string(),
        "invalid config key `scrollback`: must be a positive integer"
    );
}

#[test]
fn build_validation_error_names_the_key_and_detail() {
    let validation_error = build_validation_error("scrollback", "must be a positive integer");
    let ConfigError::Validation {
        config_key,
        validation_detail,
    } = validation_error
    else {
        panic!("expected ConfigError::Validation, got {validation_error:?}");
    };
    assert_eq!(config_key, "scrollback");
    assert_eq!(validation_detail, "must be a positive integer");
}

#[test]
fn parse_conversion_without_sub_diagnostics_uses_the_kdl_display() {
    let raw_kdl_error = KdlError {
        input: Arc::new(String::new()),
        diagnostics: Vec::new(),
    };
    let parse_diagnostic =
        ConfigParseDiagnostic::from_kdl_error(Path::new("koshi.kdl"), raw_kdl_error);
    let ConfigError::Parse {
        config_path,
        parse_error_detail,
    } = ConfigError::from(parse_diagnostic)
    else {
        panic!("expected ConfigError::Parse");
    };
    assert_eq!(config_path, "koshi.kdl");
    assert_eq!(parse_error_detail, "Failed to parse KDL document");
}

#[test]
fn config_errors_classify_as_recoverable_config_problems() {
    let validation_error = ConfigError::Validation {
        config_key: "scrollback".to_string(),
        validation_detail: "x".to_string(),
    };
    assert_eq!(validation_error.category(), DomainCategory::Config);
    assert_eq!(validation_error.get_severity(), Severity::Recoverable);
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

#[test]
fn color_parse_errors_compare_by_value() {
    assert_eq!(
        ColorParseError::BadLength { character_count: 5 },
        ColorParseError::BadLength { character_count: 5 }
    );
    assert_ne!(
        ColorParseError::BadLength { character_count: 5 },
        ColorParseError::BadLength { character_count: 4 }
    );
    assert_ne!(
        ColorParseError::BadLength { character_count: 6 },
        ColorParseError::BadDigit {
            invalid_hex_text: "z".to_string(),
        }
    );
}
