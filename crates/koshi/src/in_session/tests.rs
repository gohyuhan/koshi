//! Unit tests for in-session environment detection.

use super::*;

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
        ("KOSHI_SOCKET", "/run/koshi/session-x.sock"),
    ]))
    .expect("full environment parses");
    assert_eq!(
        context,
        Some(InSessionContext {
            session_id: SessionId::from_uuid(SESSION_UUID.parse().expect("uuid")),
            client_id: Some(ClientId::from_uuid(CLIENT_UUID.parse().expect("uuid"))),
            pane_id: PaneId::from_uuid(PANE_UUID.parse().expect("uuid")),
            socket: Some("/run/koshi/session-x.sock".to_string()),
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
        ("KOSHI_SOCKET", "/run/koshi/session-x.sock"),
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

/// The optional socket address may be absent; the identity still builds.
#[test]
fn absent_socket_is_allowed() {
    let context = InSessionContext::from_lookup(lookup(&[
        ("KOSHI", "1"),
        (
            "KOSHI_SESSION_ID",
            "session-0192f0c1-0000-7000-8000-000000000001",
        ),
        ("KOSHI_PANE_ID", "pane-0192f0c1-0000-7000-8000-000000000003"),
    ]))
    .expect("absent socket parses")
    .expect("in-session");
    assert_eq!(context.socket, None);
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
