//! Unit tests for [`add_koshi_environment`]: the full variable set, the prefixed id
//! forms, the omitted-variable cases (no designated client, no runtime
//! directory), and the stand-in clipboard tool folder on `PATH` on Linux and
//! macOS. On Windows the folder does not go on `PATH`.

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

/// The variables [`add_koshi_environment`] adds to an empty overlay, with the
/// stand-in clipboard tools off.
fn build_identity_environment(
    session_id: SessionId,
    client_id: Option<ClientId>,
    pane_id: PaneId,
    runtime_directory_path: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut environment_variables = BTreeMap::new();
    add_koshi_environment(
        &mut environment_variables,
        session_id,
        client_id,
        pane_id,
        runtime_directory_path,
        false,
    );
    environment_variables
}

#[test]
fn all_five_variables_with_a_client_and_a_runtime_directory() {
    let (session_id, client_id, pane_id) = build_test_identifiers();
    let environment_variables = build_identity_environment(
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
    let environment_variables = build_identity_environment(
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
    let environment_variables = build_identity_environment(
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
        build_identity_environment(session_id, None, pane_id, Some(Path::new("/run/koshi")));

    assert!(!environment_variables.contains_key("KOSHI_CLIENT_ID"));
    assert_eq!(
        environment_variables.keys().collect::<Vec<_>>(),
        ["KOSHI", "KOSHI_PANE_ID", "KOSHI_SESSION_ID", "KOSHI_SOCKET"]
    );
}

#[test]
fn no_client_and_no_runtime_directory_leaves_only_the_three_always_present_variables() {
    let (session_id, _, pane_id) = build_test_identifiers();
    let environment_variables = build_identity_environment(session_id, None, pane_id, None);

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
    let environment_variables =
        build_identity_environment(session_id, Some(client_id), pane_id, None);

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

/// The overlay of a spec whose own `PATH` is `spec_path`, after
/// [`add_koshi_environment`] with the stand-in clipboard tools on and the
/// runtime directory `runtime_directory_path`.
fn build_environment_with_clipboard_tools(
    spec_path: Option<&str>,
    runtime_directory_path: Option<&Path>,
) -> BTreeMap<String, String> {
    let (session_id, client_id, pane_id) = build_test_identifiers();
    let mut environment_variables = BTreeMap::new();
    if let Some(spec_path) = spec_path {
        environment_variables.insert("PATH".to_string(), spec_path.to_string());
    }
    add_koshi_environment(
        &mut environment_variables,
        session_id,
        Some(client_id),
        pane_id,
        runtime_directory_path,
        true,
    );
    environment_variables
}

#[cfg(unix)]
#[test]
fn clipboard_tools_put_the_session_folder_in_front_of_the_spec_path() {
    let environment_variables = build_environment_with_clipboard_tools(
        Some("/usr/bin:/bin"),
        Some(Path::new("/run/koshi")),
    );
    assert_eq!(
        environment_variables.get("PATH").map(String::as_str),
        Some(
            "/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.clipboard-tools:/usr/bin:/bin"
        )
    );
}

#[cfg(unix)]
#[test]
fn clipboard_tools_put_the_session_folder_in_front_of_the_process_path_when_the_spec_has_none() {
    let environment_variables =
        build_environment_with_clipboard_tools(None, Some(Path::new("/run/koshi")));
    let expected_path = match std::env::var("PATH") {
        Ok(process_path) if !process_path.is_empty() => Some(format!(
            "/run/koshi/session-0198a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.clipboard-tools:{process_path}"
        )),
        _ => None,
    };
    assert_eq!(environment_variables.get("PATH").cloned(), expected_path);
}

#[cfg(windows)]
#[test]
fn clipboard_tools_leave_the_path_as_it_is_on_windows() {
    let environment_variables = build_environment_with_clipboard_tools(
        Some(r"C:\Windows\System32"),
        Some(Path::new(r"C:\unused")),
    );
    assert_eq!(
        environment_variables.get("PATH").map(String::as_str),
        Some(r"C:\Windows\System32")
    );
}

#[cfg(unix)]
#[test]
fn clipboard_tools_leave_an_empty_spec_path_as_it_is() {
    let environment_variables =
        build_environment_with_clipboard_tools(Some(""), Some(Path::new("/run/koshi")));
    assert_eq!(
        environment_variables.get("PATH").map(String::as_str),
        Some("")
    );
}

#[cfg(unix)]
#[test]
fn clipboard_tools_leave_the_path_as_it_is_for_a_runtime_directory_that_holds_a_colon() {
    let environment_variables = build_environment_with_clipboard_tools(
        Some("/usr/bin:/bin"),
        Some(Path::new("/run/ko:shi")),
    );
    assert_eq!(
        environment_variables.get("PATH").map(String::as_str),
        Some("/usr/bin:/bin")
    );
}

#[cfg(unix)]
#[test]
fn clipboard_tools_leave_the_path_as_it_is_for_a_runtime_directory_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt;

    let environment_variables = build_environment_with_clipboard_tools(
        Some("/usr/bin:/bin"),
        Some(Path::new(std::ffi::OsStr::from_bytes(b"/run/ko\xffshi"))),
    );
    assert_eq!(
        environment_variables.get("PATH").map(String::as_str),
        Some("/usr/bin:/bin")
    );
}

#[cfg(unix)]
#[test]
fn clipboard_tools_add_no_path_without_a_runtime_directory() {
    let environment_variables = build_environment_with_clipboard_tools(Some("/usr/bin:/bin"), None);
    assert_eq!(
        environment_variables.get("PATH").map(String::as_str),
        Some("/usr/bin:/bin")
    );
}

#[test]
fn clipboard_tools_off_leave_the_spec_path_as_it_is() {
    let (session_id, client_id, pane_id) = build_test_identifiers();
    let mut environment_variables =
        BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
    add_koshi_environment(
        &mut environment_variables,
        session_id,
        Some(client_id),
        pane_id,
        Some(Path::new("/run/koshi")),
        false,
    );
    assert_eq!(
        environment_variables.get("PATH").map(String::as_str),
        Some("/usr/bin:/bin")
    );
}
