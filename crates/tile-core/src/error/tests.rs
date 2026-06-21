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
        (DomainCategory::Plugin, "plugin"),
        (DomainCategory::Storage, "storage"),
    ];
    for (cat, want) in &cases {
        assert_eq!(cat.to_string(), *want);
    }
    assert_eq!(cases.len(), 8);
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
