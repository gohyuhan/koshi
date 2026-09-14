//! Tests for error categories and severity.

use super::*;
use serde::de::DeserializeOwned;
use serde::Serialize;

fn assert_serde_roundtrip<Roundtrippable>(serializable_value: &Roundtrippable)
where
    Roundtrippable: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let serialized_json = serde_json::to_string(serializable_value).expect("serialize");
    let deserialized_roundtrippable: Roundtrippable =
        serde_json::from_str(&serialized_json).expect("deserialize");
    assert_eq!(*serializable_value, deserialized_roundtrippable);
}

#[test]
fn domain_category_roundtrips() {
    let category_values = [
        DomainCategory::Config,
        DomainCategory::Cli,
        DomainCategory::Ipc,
        DomainCategory::Pty,
        DomainCategory::Terminal,
        DomainCategory::Layout,
        DomainCategory::Plugin,
        DomainCategory::Session,
        DomainCategory::Storage,
    ];
    for category in &category_values {
        assert_serde_roundtrip(category);
    }
    assert_eq!(category_values.len(), 9);
}

#[test]
fn severity_roundtrips() {
    let severity_values = [
        Severity::Recoverable,
        Severity::ClientFatal,
        Severity::SessionFatal,
        Severity::ProcessFatal,
    ];
    for severity in &severity_values {
        assert_serde_roundtrip(severity);
    }
    assert_eq!(severity_values.len(), 4);
}

#[test]
fn severity_orders_least_to_most_fatal() {
    assert!(Severity::Recoverable < Severity::ClientFatal);
    assert!(Severity::ClientFatal < Severity::SessionFatal);
    assert!(Severity::SessionFatal < Severity::ProcessFatal);
}

#[test]
fn category_display_is_human() {
    let category_display_cases = [
        (DomainCategory::Config, "config"),
        (DomainCategory::Cli, "cli"),
        (DomainCategory::Ipc, "ipc"),
        (DomainCategory::Pty, "pty"),
        (DomainCategory::Terminal, "terminal"),
        (DomainCategory::Layout, "layout"),
        (DomainCategory::Session, "session"),
        (DomainCategory::Plugin, "plugin"),
        (DomainCategory::Storage, "storage"),
    ];
    for (category, expected_display) in &category_display_cases {
        assert_eq!(category.to_string(), *expected_display);
    }
    assert_eq!(category_display_cases.len(), 9);
}

#[test]
fn severity_display_is_human() {
    let severity_display_cases = [
        (Severity::Recoverable, "recoverable"),
        (Severity::ClientFatal, "client-fatal"),
        (Severity::SessionFatal, "session-fatal"),
        (Severity::ProcessFatal, "process-fatal"),
    ];
    for (severity, expected_display) in &severity_display_cases {
        assert_eq!(severity.to_string(), *expected_display);
    }
    assert_eq!(severity_display_cases.len(), 4);
}

#[test]
fn domain_category_serializes_as_its_variant_name() {
    let category_json_cases = [
        (DomainCategory::Config, "\"Config\""),
        (DomainCategory::Cli, "\"Cli\""),
        (DomainCategory::Ipc, "\"Ipc\""),
        (DomainCategory::Pty, "\"Pty\""),
        (DomainCategory::Terminal, "\"Terminal\""),
        (DomainCategory::Layout, "\"Layout\""),
        (DomainCategory::Plugin, "\"Plugin\""),
        (DomainCategory::Session, "\"Session\""),
        (DomainCategory::Storage, "\"Storage\""),
    ];
    for (category, expected_json) in &category_json_cases {
        assert_eq!(
            serde_json::to_string(category).expect("serialize"),
            *expected_json
        );
    }
    assert_eq!(category_json_cases.len(), 9);
}

#[test]
fn severity_serializes_as_its_variant_name() {
    let severity_json_cases = [
        (Severity::Recoverable, "\"Recoverable\""),
        (Severity::ClientFatal, "\"ClientFatal\""),
        (Severity::SessionFatal, "\"SessionFatal\""),
        (Severity::ProcessFatal, "\"ProcessFatal\""),
    ];
    for (severity, expected_json) in &severity_json_cases {
        assert_eq!(
            serde_json::to_string(severity).expect("serialize"),
            *expected_json
        );
    }
    assert_eq!(severity_json_cases.len(), 4);
}

#[test]
fn an_unknown_category_name_is_rejected() {
    let category_parse_error =
        serde_json::from_str::<DomainCategory>("\"Network\"").expect_err("rejects");

    assert_eq!(
        category_parse_error.to_string(),
        "unknown variant `Network`, expected one of `Config`, `Cli`, `Ipc`, `Pty`, `Terminal`, `Layout`, `Plugin`, `Session`, `Storage` at line 1 column 9"
    );
}

#[test]
fn an_unknown_severity_name_is_rejected() {
    let severity_parse_error = serde_json::from_str::<Severity>("\"Fatal\"").expect_err("rejects");

    assert_eq!(
        severity_parse_error.to_string(),
        "unknown variant `Fatal`, expected one of `Recoverable`, `ClientFatal`, `SessionFatal`, `ProcessFatal` at line 1 column 7"
    );
}

#[test]
fn process_fatal_is_the_most_fatal_severity() {
    let severities = [
        Severity::Recoverable,
        Severity::ClientFatal,
        Severity::SessionFatal,
        Severity::ProcessFatal,
    ];

    assert_eq!(severities.iter().max(), Some(&Severity::ProcessFatal));
    assert_eq!(severities.iter().min(), Some(&Severity::Recoverable));
}
