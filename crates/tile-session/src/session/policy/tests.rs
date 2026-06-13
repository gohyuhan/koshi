use super::EmptyTabPolicy;

#[test]
fn the_default_empty_tab_policy_closes_the_tab() {
    assert_eq!(EmptyTabPolicy::default(), EmptyTabPolicy::CloseTab);
}

#[test]
fn an_empty_tab_policy_survives_a_serde_round_trip() {
    for policy in [
        EmptyTabPolicy::KeepDeadPlaceholder,
        EmptyTabPolicy::RespawnShell,
        EmptyTabPolicy::CloseTab,
    ] {
        let json = serde_json::to_string(&policy).expect("serialize");
        let restored: EmptyTabPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(policy, restored);
    }
}
