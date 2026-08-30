//! Tests for empty-tab and last-tab policy enums: serialization and defaults.

use super::{EmptyTabPolicy, LastTabPolicy};

#[test]
fn the_default_empty_tab_policy_closes_the_tab() {
    assert_eq!(EmptyTabPolicy::default(), EmptyTabPolicy::CloseTab);
}

#[test]
fn the_default_last_tab_policy_quits() {
    assert_eq!(LastTabPolicy::default(), LastTabPolicy::Quit);
}

#[test]
fn a_last_tab_policy_survives_a_serde_round_trip() {
    let policy = LastTabPolicy::Quit;
    let json = serde_json::to_string(&policy).expect("serialize");
    let restored: LastTabPolicy = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(policy, restored);
}

#[test]
fn an_empty_tab_policy_survives_a_serde_round_trip() {
    for policy in [EmptyTabPolicy::RespawnShell, EmptyTabPolicy::CloseTab] {
        let json = serde_json::to_string(&policy).expect("serialize");
        let restored: EmptyTabPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(policy, restored);
    }
}

#[test]
fn the_empty_tab_policies_serialize_as_their_variant_names() {
    assert_eq!(
        serde_json::to_string(&EmptyTabPolicy::RespawnShell).expect("serialize"),
        r#""RespawnShell""#
    );
    assert_eq!(
        serde_json::to_string(&EmptyTabPolicy::CloseTab).expect("serialize"),
        r#""CloseTab""#
    );
}

#[test]
fn the_last_tab_policy_serializes_as_its_variant_name() {
    assert_eq!(
        serde_json::to_string(&LastTabPolicy::Quit).expect("serialize"),
        r#""Quit""#
    );
}

#[test]
fn an_unknown_empty_tab_policy_fails_to_deserialize() {
    let error =
        serde_json::from_str::<EmptyTabPolicy>(r#""KeepOpen""#).expect_err("unknown variant");

    assert_eq!(
        error.to_string(),
        "unknown variant `KeepOpen`, expected `RespawnShell` or `CloseTab` at line 1 column 10"
    );
}

#[test]
fn an_unknown_last_tab_policy_fails_to_deserialize() {
    let error = serde_json::from_str::<LastTabPolicy>(r#""Detach""#).expect_err("unknown variant");

    assert_eq!(
        error.to_string(),
        "unknown variant `Detach`, expected `Quit` at line 1 column 8"
    );
}

#[test]
fn a_non_string_empty_tab_policy_fails_to_deserialize() {
    let error = serde_json::from_str::<EmptyTabPolicy>("0").expect_err("wrong json type");

    assert_eq!(error.to_string(), "expected value at line 1 column 1");
}
