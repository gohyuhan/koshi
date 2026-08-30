//! Tests for error categories and severity.

use super::*;
use serde::de::DeserializeOwned;
use serde::Serialize;

fn roundtrip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let back: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(*value, back);
}

#[test]
fn domain_category_roundtrips() {
    let cases = [
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
    for c in &cases {
        roundtrip(c);
    }
    assert_eq!(cases.len(), 9);
}

#[test]
fn severity_roundtrips() {
    let cases = [
        Severity::Recoverable,
        Severity::ClientFatal,
        Severity::SessionFatal,
        Severity::ProcessFatal,
    ];
    for s in &cases {
        roundtrip(s);
    }
    assert_eq!(cases.len(), 4);
}

#[test]
fn severity_orders_least_to_most_fatal() {
    assert!(Severity::Recoverable < Severity::ClientFatal);
    assert!(Severity::ClientFatal < Severity::SessionFatal);
    assert!(Severity::SessionFatal < Severity::ProcessFatal);
}

#[test]
fn category_display_is_human() {
    let cases = [
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
    for (cat, want) in &cases {
        assert_eq!(cat.to_string(), *want);
    }
    assert_eq!(cases.len(), 9);
}

#[test]
fn severity_display_is_human() {
    let cases = [
        (Severity::Recoverable, "recoverable"),
        (Severity::ClientFatal, "client-fatal"),
        (Severity::SessionFatal, "session-fatal"),
        (Severity::ProcessFatal, "process-fatal"),
    ];
    for (sev, want) in &cases {
        assert_eq!(sev.to_string(), *want);
    }
    assert_eq!(cases.len(), 4);
}

#[test]
fn domain_category_serializes_as_its_variant_name() {
    let cases = [
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
    for (cat, want) in &cases {
        assert_eq!(serde_json::to_string(cat).expect("serialize"), *want);
    }
    assert_eq!(cases.len(), 9);
}

#[test]
fn severity_serializes_as_its_variant_name() {
    let cases = [
        (Severity::Recoverable, "\"Recoverable\""),
        (Severity::ClientFatal, "\"ClientFatal\""),
        (Severity::SessionFatal, "\"SessionFatal\""),
        (Severity::ProcessFatal, "\"ProcessFatal\""),
    ];
    for (sev, want) in &cases {
        assert_eq!(serde_json::to_string(sev).expect("serialize"), *want);
    }
    assert_eq!(cases.len(), 4);
}

#[test]
fn an_unknown_category_name_is_rejected() {
    let err = serde_json::from_str::<DomainCategory>("\"Network\"").expect_err("rejects");

    assert_eq!(
        err.to_string(),
        "unknown variant `Network`, expected one of `Config`, `Cli`, `Ipc`, `Pty`, `Terminal`, `Layout`, `Plugin`, `Session`, `Storage` at line 1 column 9"
    );
}

#[test]
fn an_unknown_severity_name_is_rejected() {
    let err = serde_json::from_str::<Severity>("\"Fatal\"").expect_err("rejects");

    assert_eq!(
        err.to_string(),
        "unknown variant `Fatal`, expected one of `Recoverable`, `ClientFatal`, `SessionFatal`, `ProcessFatal` at line 1 column 7"
    );
}

#[test]
fn process_fatal_is_the_most_fatal_severity() {
    let all = [
        Severity::Recoverable,
        Severity::ClientFatal,
        Severity::SessionFatal,
        Severity::ProcessFatal,
    ];

    assert_eq!(all.iter().max(), Some(&Severity::ProcessFatal));
    assert_eq!(all.iter().min(), Some(&Severity::Recoverable));
}
