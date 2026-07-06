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
