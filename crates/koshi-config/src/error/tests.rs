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
