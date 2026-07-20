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
    // The confirm is a UI step; once confirmed, the close is graceful (3s).
    assert_eq!(
        PaneClosePolicy::ConfirmIfBusy.kill_policy(),
        KillPolicy::Graceful {
            timeout: Duration::from_secs(3)
        }
    );
    // No close policy ever escalates to a whole-tree kill.
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

    // `duration_secs` encodes the timeout as a plain integer, so the on-disk
    // form is a whole second and identical to `KillPolicy`'s.
    assert_eq!(close, r#"{"Graceful":{"timeout":3}}"#);
    assert_eq!(close, kill);
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
