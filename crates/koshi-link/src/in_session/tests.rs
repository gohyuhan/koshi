//! Unit tests for in-session environment detection.

use super::*;

use std::cell::RefCell;
use std::collections::BTreeMap;

const SESSION_UUID: &str = "0192f0c1-0000-7000-8000-000000000001";
const CLIENT_UUID: &str = "0192f0c1-0000-7000-8000-000000000002";
const PANE_UUID: &str = "0192f0c1-0000-7000-8000-000000000003";

/// Build a lookup over a fixed variable map.
fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = vars
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    move |name| map.get(name).cloned()
}

/// The full injected environment yields the full identity.
#[test]
fn full_environment_builds_the_full_identity() {
    let context = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        (
            "KOSHI_SESSION_ID",
            "session-0192f0c1-0000-7000-8000-000000000001",
        ),
        (
            "KOSHI_CLIENT_ID",
            "client-0192f0c1-0000-7000-8000-000000000002",
        ),
        ("KOSHI_PANE_ID", "pane-0192f0c1-0000-7000-8000-000000000003"),
    ]))
    .expect("full environment parses");
    assert_eq!(
        context,
        Some(InSessionContext {
            session_id: SessionId::from_uuid(SESSION_UUID.parse().expect("uuid")),
            client_id: Some(ClientId::from_uuid(CLIENT_UUID.parse().expect("uuid"))),
            pane_id: PaneId::from_uuid(PANE_UUID.parse().expect("uuid")),
        })
    );
}

/// No `KOSHI` marker is external mode, even when other `KOSHI_*` variables
/// linger in the environment.
#[test]
fn absent_marker_is_external_mode() {
    let context = InSessionContext::from_lookup(lookup(&[(
        "KOSHI_SESSION_ID",
        "session-0192f0c1-0000-7000-8000-000000000001",
    )]))
    .expect("no marker parses");
    assert_eq!(context, None);
}

/// Presence of `KOSHI` is the marker: any value, including empty, claims
/// in-session identity and requires the rest of the variables.
#[test]
fn empty_marker_still_claims_in_session_identity() {
    let error = InSessionContext::from_lookup(lookup(&[("KOSHI", "")]))
        .expect_err("marker without identity is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI` is set but `KOSHI_SESSION_ID` is missing"
    );
}

/// A missing required session id is rejected, not treated as external mode.
#[test]
fn missing_session_id_is_rejected() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        ("KOSHI_PANE_ID", "pane-0192f0c1-0000-7000-8000-000000000003"),
    ]))
    .expect_err("missing session id is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI` is set but `KOSHI_SESSION_ID` is missing"
    );
}

/// A missing required pane id is rejected.
#[test]
fn missing_pane_id_is_rejected() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        (
            "KOSHI_SESSION_ID",
            "session-0192f0c1-0000-7000-8000-000000000001",
        ),
    ]))
    .expect_err("missing pane id is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI` is set but `KOSHI_PANE_ID` is missing"
    );
}

/// A malformed session id names the variable and the offending value.
#[test]
fn malformed_session_id_is_rejected() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        ("KOSHI_SESSION_ID", "garbage"),
        ("KOSHI_PANE_ID", "pane-0192f0c1-0000-7000-8000-000000000003"),
    ]))
    .expect_err("malformed session id is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_SESSION_ID` is `garbage`: \
         expected `session-<uuid>` or a bare UUID"
    );
}

/// An id carrying the wrong entity prefix does not strip, so it is rejected.
#[test]
fn wrong_prefix_on_pane_id_is_rejected() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        (
            "KOSHI_SESSION_ID",
            "session-0192f0c1-0000-7000-8000-000000000001",
        ),
        (
            "KOSHI_PANE_ID",
            "session-0192f0c1-0000-7000-8000-000000000003",
        ),
    ]))
    .expect_err("wrong-prefix pane id is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_PANE_ID` is \
         `session-0192f0c1-0000-7000-8000-000000000003`: \
         expected `pane-<uuid>` or a bare UUID"
    );
}

/// The optional client id may be absent; the identity still builds.
#[test]
fn absent_client_id_is_allowed() {
    let context = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        (
            "KOSHI_SESSION_ID",
            "session-0192f0c1-0000-7000-8000-000000000001",
        ),
        ("KOSHI_PANE_ID", "pane-0192f0c1-0000-7000-8000-000000000003"),
    ]))
    .expect("absent client id parses")
    .expect("in-session");
    assert_eq!(context.client_id, None);
}

/// A client id that is present but malformed is rejected, never dropped.
#[test]
fn malformed_client_id_is_rejected() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        (
            "KOSHI_SESSION_ID",
            "session-0192f0c1-0000-7000-8000-000000000001",
        ),
        ("KOSHI_CLIENT_ID", "client-not-a-uuid"),
        ("KOSHI_PANE_ID", "pane-0192f0c1-0000-7000-8000-000000000003"),
    ]))
    .expect_err("malformed client id is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_CLIENT_ID` is `client-not-a-uuid`: \
         expected `client-<uuid>` or a bare UUID"
    );
}

/// Bare UUID values without the entity prefix are accepted.
#[test]
fn bare_uuid_values_are_accepted() {
    let context = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        ("KOSHI_SESSION_ID", SESSION_UUID),
        ("KOSHI_PANE_ID", PANE_UUID),
    ]))
    .expect("bare uuids parse")
    .expect("in-session");
    assert_eq!(
        context.session_id,
        SessionId::from_uuid(SESSION_UUID.parse().expect("uuid"))
    );
    assert_eq!(
        context.pane_id,
        PaneId::from_uuid(PANE_UUID.parse().expect("uuid"))
    );
}

/// A bare UUID in the optional client id is accepted, same as a prefixed one.
#[test]
fn a_bare_uuid_client_id_is_accepted() {
    let context = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        ("KOSHI_SESSION_ID", SESSION_UUID),
        ("KOSHI_CLIENT_ID", CLIENT_UUID),
        ("KOSHI_PANE_ID", PANE_UUID),
    ]))
    .expect("bare uuids parse")
    .expect("in-session");
    assert_eq!(
        context.client_id,
        Some(ClientId::from_uuid(CLIENT_UUID.parse().expect("uuid")))
    );
}

/// A required variable that is present but empty is malformed, not missing:
/// the message names the value, not the absence.
#[test]
fn an_empty_session_id_is_rejected_as_malformed() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        ("KOSHI_SESSION_ID", ""),
        ("KOSHI_PANE_ID", PANE_UUID),
    ]))
    .expect_err("an empty session id is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_SESSION_ID` is ``: \
         expected `session-<uuid>` or a bare UUID"
    );
}

/// An empty client id is present-but-malformed, never read as absent.
#[test]
fn an_empty_client_id_is_rejected() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        ("KOSHI_SESSION_ID", SESSION_UUID),
        ("KOSHI_CLIENT_ID", ""),
        ("KOSHI_PANE_ID", PANE_UUID),
    ]))
    .expect_err("an empty client id is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_CLIENT_ID` is ``: \
         expected `client-<uuid>` or a bare UUID"
    );
}

/// With both the session id malformed and the pane id missing, the session id
/// is the one reported.
#[test]
fn the_session_id_is_reported_before_the_missing_pane_id() {
    let error =
        InSessionContext::from_lookup(lookup(&[("KOSHI", "1"), ("KOSHI_SESSION_ID", "garbage")]))
            .expect_err("the session id is rejected first");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_SESSION_ID` is `garbage`: \
         expected `session-<uuid>` or a bare UUID"
    );
}

/// With both the client id malformed and the pane id missing, the client id is
/// the one reported.
#[test]
fn the_client_id_is_reported_before_the_missing_pane_id() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        ("KOSHI_SESSION_ID", SESSION_UUID),
        ("KOSHI_CLIENT_ID", "garbage"),
    ]))
    .expect_err("the client id is rejected first");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_CLIENT_ID` is `garbage`: \
         expected `client-<uuid>` or a bare UUID"
    );
}

/// Upper-case hex digits and a UUID written without hyphens both read as the
/// same id.
#[test]
fn upper_case_and_unhyphenated_uuids_are_accepted() {
    let context = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        (
            "KOSHI_SESSION_ID",
            "session-0192F0C1-0000-7000-8000-000000000001",
        ),
        ("KOSHI_PANE_ID", "pane-0192f0c1000070008000000000000003"),
    ]))
    .expect("upper-case and unhyphenated uuids parse")
    .expect("in-session");
    assert_eq!(
        context.session_id,
        SessionId::from_uuid(SESSION_UUID.parse().expect("uuid"))
    );
    assert_eq!(
        context.pane_id,
        PaneId::from_uuid(PANE_UUID.parse().expect("uuid"))
    );
}

/// A value with a leading space is not trimmed, so it is rejected and echoed
/// with the space.
#[test]
fn a_value_with_a_leading_space_is_rejected() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        ("KOSHI_SESSION_ID", SESSION_UUID),
        (
            "KOSHI_PANE_ID",
            " pane-0192f0c1-0000-7000-8000-000000000003",
        ),
    ]))
    .expect_err("a padded pane id is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_PANE_ID` is \
         ` pane-0192f0c1-0000-7000-8000-000000000003`: \
         expected `pane-<uuid>` or a bare UUID"
    );
}

/// The entity prefix only strips with its `-` separator, so a value that runs
/// the prefix into the UUID is rejected.
#[test]
fn a_prefix_without_its_separator_is_rejected() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        (
            "KOSHI_SESSION_ID",
            "session0192f0c1-0000-7000-8000-000000000001",
        ),
        ("KOSHI_PANE_ID", PANE_UUID),
    ]))
    .expect_err("a prefix with no separator is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_SESSION_ID` is \
         `session0192f0c1-0000-7000-8000-000000000001`: \
         expected `session-<uuid>` or a bare UUID"
    );
}

/// The entity prefix strips once, so a value carrying it twice is rejected.
#[test]
fn a_doubled_entity_prefix_is_rejected() {
    let error = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        ("KOSHI_SESSION_ID", SESSION_UUID),
        (
            "KOSHI_PANE_ID",
            "pane-pane-0192f0c1-0000-7000-8000-000000000003",
        ),
    ]))
    .expect_err("a doubled prefix is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_PANE_ID` is \
         `pane-pane-0192f0c1-0000-7000-8000-000000000003`: \
         expected `pane-<uuid>` or a bare UUID"
    );
}

/// Each variable is read once, in this order: the marker, the session, the
/// client, then the pane.
#[test]
fn each_variable_is_read_once_and_in_order() {
    let vars = lookup(&[
        ("KOSHI", "1"),
        ("KOSHI_SESSION_ID", SESSION_UUID),
        ("KOSHI_CLIENT_ID", CLIENT_UUID),
        ("KOSHI_PANE_ID", PANE_UUID),
    ]);
    let reads = RefCell::new(Vec::new());
    InSessionContext::from_lookup(|name| {
        reads.borrow_mut().push(name.to_string());
        vars(name)
    })
    .expect("full environment parses")
    .expect("in-session");
    assert_eq!(
        reads.into_inner(),
        [
            "KOSHI",
            "KOSHI_SESSION_ID",
            "KOSHI_CLIENT_ID",
            "KOSHI_PANE_ID"
        ]
    );
}

/// A malformed session id ends the read: the client and pane variables are
/// never looked up.
#[test]
fn a_malformed_session_id_stops_before_the_client_and_pane_are_read() {
    let vars = lookup(&[("KOSHI", "1"), ("KOSHI_SESSION_ID", "garbage")]);
    let reads = RefCell::new(Vec::new());
    let error = InSessionContext::from_lookup(|name| {
        reads.borrow_mut().push(name.to_string());
        vars(name)
    })
    .expect_err("the session id is rejected");
    assert_eq!(
        error.to_string(),
        "broken in-session environment: `KOSHI_SESSION_ID` is `garbage`: \
         expected `session-<uuid>` or a bare UUID"
    );
    assert_eq!(reads.into_inner(), ["KOSHI", "KOSHI_SESSION_ID"]);
}

/// The marker's value is never inspected: `KOSHI=0` claims in-session identity
/// exactly as `KOSHI=1` does.
#[test]
fn a_marker_holding_zero_still_claims_in_session_identity() {
    let context = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "0"),
        ("KOSHI_SESSION_ID", SESSION_UUID),
        ("KOSHI_PANE_ID", PANE_UUID),
    ]))
    .expect("the marker's value is not inspected");
    assert_eq!(
        context,
        Some(InSessionContext {
            session_id: SessionId::from_uuid(SESSION_UUID.parse().expect("uuid")),
            client_id: None,
            pane_id: PaneId::from_uuid(PANE_UUID.parse().expect("uuid")),
        })
    );
}
