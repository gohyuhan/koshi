//! Unit tests for [`build_koshi_environment`]: the full variable set, the prefixed id
//! forms, and the omitted-variable cases (no designated client, no runtime
//! directory).

use super::*;

use uuid::Uuid;

fn build_test_identifiers() -> (SessionId, ClientId, PaneId) {
    let session_id = SessionId::from_uuid(
        Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid"),
    );
    let client_id = ClientId::from_uuid(
        Uuid::parse_str("11111111-2222-3333-4444-555555555555").expect("valid uuid"),
    );
    let pane_id = PaneId::from_uuid(
        Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").expect("valid uuid"),
    );
    (session_id, client_id, pane_id)
}

#[test]
fn all_five_variables_with_a_client_and_a_runtime_directory() {
    let (session_id, client_id, pane_id) = build_test_identifiers();
    let environment_variables = build_koshi_environment(
        session_id,
        Some(client_id),
        pane_id,
        Some(Path::new("/run/koshi")),
    );

    let expected_environment_variables: BTreeMap<String, String> = [
        ("KOSHI", "1"),
        (
            "KOSHI_SESSION_ID",
            "session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b",
        ),
        (
            "KOSHI_CLIENT_ID",
            "client-11111111-2222-3333-4444-555555555555",
        ),
        ("KOSHI_PANE_ID", "pane-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
        (
            "KOSHI_SOCKET",
            compute_socket_address(Path::new("/run/koshi"), session_id).as_str(),
        ),
    ]
    .into_iter()
    .map(|(environment_variable_name, environment_variable_value)| {
        (
            environment_variable_name.to_string(),
            environment_variable_value.to_string(),
        )
    })
    .collect();

    assert_eq!(environment_variables, expected_environment_variables);
}

#[cfg(unix)]
#[test]
fn the_socket_variable_is_the_session_socket_path() {
    let (session_id, client_id, pane_id) = build_test_identifiers();
    let environment_variables = build_koshi_environment(
        session_id,
        Some(client_id),
        pane_id,
        Some(Path::new("/run/koshi")),
    );
    assert_eq!(
        environment_variables
            .get("KOSHI_SOCKET")
            .expect("socket variable"),
        "/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.sock"
    );
}

#[cfg(windows)]
#[test]
fn the_socket_variable_is_the_session_pipe_name() {
    let (session_id, client_id, pane_id) = build_test_identifiers();
    let environment_variables = build_koshi_environment(
        session_id,
        Some(client_id),
        pane_id,
        Some(Path::new(r"C:\unused")),
    );
    assert_eq!(
        environment_variables
            .get("KOSHI_SOCKET")
            .expect("socket variable"),
        "koshi-session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
    );
}

#[test]
fn no_designated_client_omits_the_client_variable() {
    let (session_id, _, pane_id) = build_test_identifiers();
    let environment_variables =
        build_koshi_environment(session_id, None, pane_id, Some(Path::new("/run/koshi")));

    assert!(!environment_variables.contains_key("KOSHI_CLIENT_ID"));
    assert_eq!(
        environment_variables.keys().collect::<Vec<_>>(),
        ["KOSHI", "KOSHI_PANE_ID", "KOSHI_SESSION_ID", "KOSHI_SOCKET"]
    );
}

#[test]
fn no_client_and_no_runtime_directory_leaves_only_the_three_always_present_variables() {
    let (session_id, _, pane_id) = build_test_identifiers();
    let environment_variables = build_koshi_environment(session_id, None, pane_id, None);

    let expected_environment_variables: BTreeMap<String, String> = [
        ("KOSHI", "1"),
        ("KOSHI_PANE_ID", "pane-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
        (
            "KOSHI_SESSION_ID",
            "session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b",
        ),
    ]
    .into_iter()
    .map(|(environment_variable_name, environment_variable_value)| {
        (
            environment_variable_name.to_string(),
            environment_variable_value.to_string(),
        )
    })
    .collect();

    assert_eq!(environment_variables, expected_environment_variables);
}

#[test]
fn no_runtime_directory_omits_the_socket_variable() {
    let (session_id, client_id, pane_id) = build_test_identifiers();
    let environment_variables = build_koshi_environment(session_id, Some(client_id), pane_id, None);

    assert!(!environment_variables.contains_key("KOSHI_SOCKET"));
    assert_eq!(
        environment_variables.keys().collect::<Vec<_>>(),
        [
            "KOSHI",
            "KOSHI_CLIENT_ID",
            "KOSHI_PANE_ID",
            "KOSHI_SESSION_ID"
        ]
    );
}
