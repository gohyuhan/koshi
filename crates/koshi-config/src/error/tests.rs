//! Tests for config version checking and its diagnostic.

use super::*;

use miette::Diagnostic;

use crate::types::SCHEMA_VERSION;

#[test]
fn current_version_is_accepted() {
    assert!(check_version(SCHEMA_VERSION).is_ok());
}

#[test]
fn older_version_is_accepted() {
    // Older files are not an error here — migration upgrades them.
    assert!(check_version(SCHEMA_VERSION - 1).is_ok());
}

#[test]
fn newer_version_is_rejected() {
    let err = check_version(SCHEMA_VERSION + 1).expect_err("newer version must fail");
    assert_eq!(err.found, SCHEMA_VERSION + 1);
    assert_eq!(err.supported, SCHEMA_VERSION);
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
fn version_diagnostic_offers_an_upgrade_hint() {
    let err = check_version(SCHEMA_VERSION + 1).expect_err("newer version must fail");
    let help = err.help().expect("diagnostic has a help line").to_string();
    assert_eq!(
        help,
        "upgrade koshi to a build that understands this config"
    );
}

#[test]
fn not_found_error_names_the_path() {
    let err = ConfigError::NotFound {
        path: "/etc/koshi.kdl".to_string(),
    };
    assert_eq!(err.to_string(), "config file not found: /etc/koshi.kdl");
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
fn config_errors_classify_as_recoverable_config_problems() {
    let err = ConfigError::NotFound {
        path: "x".to_string(),
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
