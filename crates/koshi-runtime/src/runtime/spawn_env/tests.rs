//! Unit tests for [`koshi_env`]: the full variable set, the prefixed id
//! forms, and the omitted-variable cases (no designated client, no runtime
//! directory).

use super::*;

use uuid::Uuid;

fn ids() -> (SessionId, ClientId, PaneId) {
    let session = SessionId::from_uuid(
        Uuid::parse_str("0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").expect("valid uuid"),
    );
    let client = ClientId::from_uuid(
        Uuid::parse_str("11111111-2222-3333-4444-555555555555").expect("valid uuid"),
    );
    let pane = PaneId::from_uuid(
        Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").expect("valid uuid"),
    );
    (session, client, pane)
}

#[test]
fn all_five_variables_with_a_client_and_a_runtime_dir() {
    let (session, client, pane) = ids();
    let env = koshi_env(session, Some(client), pane, Some(Path::new("/run/koshi")));

    let expected: BTreeMap<String, String> = [
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
            socket_addr(Path::new("/run/koshi"), session).as_str(),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();

    assert_eq!(env, expected);
}

#[cfg(unix)]
#[test]
fn the_socket_variable_is_the_session_socket_path() {
    let (session, client, pane) = ids();
    let env = koshi_env(session, Some(client), pane, Some(Path::new("/run/koshi")));
    assert_eq!(
        env.get("KOSHI_SOCKET").expect("socket variable"),
        "/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.sock"
    );
}

#[cfg(windows)]
#[test]
fn the_socket_variable_is_the_session_pipe_name() {
    let (session, client, pane) = ids();
    let env = koshi_env(session, Some(client), pane, Some(Path::new(r"C:\unused")));
    assert_eq!(
        env.get("KOSHI_SOCKET").expect("socket variable"),
        "koshi-session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
    );
}

#[test]
fn no_designated_client_omits_the_client_variable() {
    let (session, _, pane) = ids();
    let env = koshi_env(session, None, pane, Some(Path::new("/run/koshi")));

    assert!(!env.contains_key("KOSHI_CLIENT_ID"));
    assert_eq!(
        env.keys().collect::<Vec<_>>(),
        ["KOSHI", "KOSHI_PANE_ID", "KOSHI_SESSION_ID", "KOSHI_SOCKET"]
    );
}

#[test]
fn no_runtime_dir_omits_the_socket_variable() {
    let (session, client, pane) = ids();
    let env = koshi_env(session, Some(client), pane, None);

    assert!(!env.contains_key("KOSHI_SOCKET"));
    assert_eq!(
        env.keys().collect::<Vec<_>>(),
        [
            "KOSHI",
            "KOSHI_CLIENT_ID",
            "KOSHI_PANE_ID",
            "KOSHI_SESSION_ID"
        ]
    );
}
