//! Tests for rendering the two version answers.

use super::*;

use clap::CommandFactory;
use koshi_core::ids::SessionId;

use crate::cli::Cli;

/// A session id whose text is fixed, so a rendered table can be compared
/// character for character.
fn session(tag: u128) -> SessionId {
    SessionId::from_uuid(uuid::Uuid::from_u128(tag))
}

/// Every state a server row can be in, against one router and three sessions.
fn mixed_rows() -> Vec<ServerVersionRow> {
    vec![
        ServerVersionRow {
            kind: ServerKind::Router,
            session: None,
            build: ServerBuild::Running {
                version: "0.2.0".to_string(),
            },
        },
        ServerVersionRow {
            kind: ServerKind::Session,
            session: Some(session(1)),
            build: ServerBuild::Unnamed,
        },
        ServerVersionRow {
            kind: ServerKind::Session,
            session: Some(session(2)),
            build: ServerBuild::NotRunning,
        },
        ServerVersionRow {
            kind: ServerKind::Session,
            session: Some(session(3)),
            build: ServerBuild::Unreachable {
                detail: "the socket closed mid-answer".to_string(),
            },
        },
    ]
}

#[test]
fn the_version_table_is_the_line_the_version_flag_prints() {
    let rendered = render_client_version(&ClientVersion::of_this_build(), FormatArg::Table);

    assert_eq!(rendered, Cli::command().render_version());
    assert_eq!(rendered, format!("koshi {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn the_version_json_carries_the_build_alone() {
    let rendered = render_client_version(
        &ClientVersion {
            version: "0.2.0".to_string(),
        },
        FormatArg::Json,
    );

    assert_eq!(rendered, "{\n  \"version\": \"0.2.0\"\n}\n");
}

#[test]
fn a_server_table_tells_every_state_apart() {
    let rendered = render_server_versions(&mixed_rows(), FormatArg::Table);

    assert_eq!(
        rendered,
        "kind     session                                       version\n\
         router   -                                             0.2.0\n\
         session  session-00000000-0000-0000-0000-000000000001  unknown\n\
         session  session-00000000-0000-0000-0000-000000000002  not running\n\
         session  session-00000000-0000-0000-0000-000000000003  unreachable\n"
    );
}

#[test]
fn a_server_json_answer_keeps_every_state_apart() {
    let rendered = render_server_versions(&mixed_rows(), FormatArg::Json);

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&rendered).expect("the answer is JSON"),
        serde_json::json!([
            { "kind": "router", "session": null, "state": "running", "version": "0.2.0" },
            {
                "kind": "session",
                "session": "00000000-0000-0000-0000-000000000001",
                "state": "unnamed"
            },
            {
                "kind": "session",
                "session": "00000000-0000-0000-0000-000000000002",
                "state": "not_running"
            },
            {
                "kind": "session",
                "session": "00000000-0000-0000-0000-000000000003",
                "state": "unreachable",
                "detail": "the socket closed mid-answer"
            },
        ])
    );
}

#[test]
fn a_machine_running_nothing_still_renders_its_header_row() {
    let rows = vec![ServerVersionRow {
        kind: ServerKind::Router,
        session: None,
        build: ServerBuild::NotRunning,
    }];

    assert_eq!(
        render_server_versions(&rows, FormatArg::Table),
        "kind    session  version\n\
         router  -        not running\n"
    );
}
