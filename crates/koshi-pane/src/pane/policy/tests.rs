//! Tests for pane close and exit policies: defaults, policy-to-kill-policy
//! mapping, and serialization round-trips.

use std::time::Duration;

use koshi_core::process::KillPolicy;

use super::{PaneClosePolicy, PaneExitPolicy};

#[test]
fn the_default_close_policy_is_a_three_second_graceful_close() {
    assert_eq!(
        PaneClosePolicy::default(),
        PaneClosePolicy::Graceful {
            timeout: Duration::from_secs(3)
        }
    );
}

#[test]
fn the_default_exit_policy_closes_the_pane_on_exit() {
    assert_eq!(PaneExitPolicy::default(), PaneExitPolicy::CloseOnExit);
}

#[test]
fn each_close_policy_maps_to_its_kill_policy() {
    // Graceful passes its own timeout straight through (5s, not the default).
    assert_eq!(
        PaneClosePolicy::Graceful {
            timeout: Duration::from_secs(5)
        }
        .kill_policy(),
        KillPolicy::Graceful {
            timeout: Duration::from_secs(5)
        }
    );
    assert_eq!(PaneClosePolicy::Force.kill_policy(), KillPolicy::Force);
    // `ConfirmIfBusy` maps to a graceful close with the default 3s timeout.
    assert_eq!(
        PaneClosePolicy::ConfirmIfBusy.kill_policy(),
        KillPolicy::Graceful {
            timeout: Duration::from_secs(3)
        }
    );
    // No close policy ever escalates to a whole-tree kill.
}

#[test]
fn a_zero_graceful_timeout_passes_through_as_zero() {
    assert_eq!(
        PaneClosePolicy::Graceful {
            timeout: Duration::ZERO
        }
        .kill_policy(),
        KillPolicy::Graceful {
            timeout: Duration::ZERO
        }
    );
}

#[test]
fn a_close_policy_survives_a_serde_round_trip() {
    for policy in [
        PaneClosePolicy::Graceful {
            timeout: Duration::from_secs(3),
        },
        PaneClosePolicy::Force,
        PaneClosePolicy::ConfirmIfBusy,
    ] {
        let json = serde_json::to_string(&policy).expect("serialize");
        let restored: PaneClosePolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(policy, restored);
    }
}

#[test]
fn a_graceful_timeout_serializes_as_whole_seconds_matching_kill_policy() {
    let timeout = Duration::from_secs(3);
    let close = serde_json::to_string(&PaneClosePolicy::Graceful { timeout }).expect("serialize");
    let kill = serde_json::to_string(&KillPolicy::Graceful { timeout }).expect("serialize");

    // `duration_secs` writes the timeout as a whole number of seconds, the same
    // form `KillPolicy` uses.
    assert_eq!(close, r#"{"Graceful":{"timeout":3}}"#);
    assert_eq!(close, kill);
}

#[test]
fn a_sub_second_graceful_timeout_loses_its_fraction_in_serde() {
    let policy = PaneClosePolicy::Graceful {
        timeout: Duration::from_millis(1500),
    };

    let json = serde_json::to_string(&policy).expect("serialize");
    let restored: PaneClosePolicy = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(json, r#"{"Graceful":{"timeout":1}}"#);
    assert_eq!(
        restored,
        PaneClosePolicy::Graceful {
            timeout: Duration::from_secs(1)
        }
    );
}

#[test]
fn the_largest_graceful_timeout_serializes_as_u64_max_seconds() {
    let policy = PaneClosePolicy::Graceful {
        timeout: Duration::MAX,
    };

    let json = serde_json::to_string(&policy).expect("serialize");
    let restored: PaneClosePolicy = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(json, r#"{"Graceful":{"timeout":18446744073709551615}}"#);
    assert_eq!(
        restored,
        PaneClosePolicy::Graceful {
            timeout: Duration::from_secs(u64::MAX)
        }
    );
}

#[test]
fn a_negative_graceful_timeout_fails_to_deserialize() {
    let error = serde_json::from_str::<PaneClosePolicy>(r#"{"Graceful":{"timeout":-1}}"#)
        .expect_err("negative seconds");

    assert_eq!(
        error.to_string(),
        "invalid value: integer `-1`, expected u64 at line 1 column 25"
    );
}

#[test]
fn a_fractional_graceful_timeout_fails_to_deserialize() {
    let error = serde_json::from_str::<PaneClosePolicy>(r#"{"Graceful":{"timeout":1.5}}"#)
        .expect_err("fractional seconds");

    assert_eq!(
        error.to_string(),
        "invalid type: floating point `1.5`, expected u64 at line 1 column 26"
    );
}

#[test]
fn an_unknown_close_policy_fails_to_deserialize() {
    let error = serde_json::from_str::<PaneClosePolicy>(r#""Kill""#).expect_err("unknown variant");

    assert_eq!(
        error.to_string(),
        "unknown variant `Kill`, expected one of `Graceful`, `Force`, `ConfirmIfBusy` at line 1 column 6"
    );
}

#[test]
fn an_exit_policy_survives_a_serde_round_trip() {
    for policy in [PaneExitPolicy::CloseOnExit, PaneExitPolicy::RespawnShell] {
        let json = serde_json::to_string(&policy).expect("serialize");
        let restored: PaneExitPolicy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(policy, restored);
    }
}

#[test]
fn the_unit_close_policies_serialize_as_their_variant_names() {
    assert_eq!(
        serde_json::to_string(&PaneClosePolicy::Force).expect("serialize"),
        r#""Force""#
    );
    assert_eq!(
        serde_json::to_string(&PaneClosePolicy::ConfirmIfBusy).expect("serialize"),
        r#""ConfirmIfBusy""#
    );
}

#[test]
fn the_exit_policies_serialize_as_their_variant_names() {
    assert_eq!(
        serde_json::to_string(&PaneExitPolicy::CloseOnExit).expect("serialize"),
        r#""CloseOnExit""#
    );
    assert_eq!(
        serde_json::to_string(&PaneExitPolicy::RespawnShell).expect("serialize"),
        r#""RespawnShell""#
    );
}

#[test]
fn an_unknown_exit_policy_fails_to_deserialize() {
    let error =
        serde_json::from_str::<PaneExitPolicy>(r#""KeepOpen""#).expect_err("unknown variant");

    assert_eq!(
        error.to_string(),
        "unknown variant `KeepOpen`, expected `CloseOnExit` or `RespawnShell` at line 1 column 10"
    );
}
