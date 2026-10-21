//! `--remote <address> --certificate-fingerprint <fingerprint>` with a server
//! this machine has not saved.
//!
//! Each test starts the real `koshi` binary under a temporary home directory,
//! with `KOSHI_REMOTE_SECRET` set, against a TLS server that the test runs on
//! a loopback port. No connection leaves this machine. A session argument that
//! names a folder of this machine's shared directory is refused before any
//! dial.

mod common;

use common::{
    build_koshi_command_under_home, build_short_test_directory, list_saved_server_records,
    spawn_stand_in_remote_server,
};
use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
use koshi_ipc::remote_wire::{RemoteServerFrame, RemoteSessionRow, REMOTE_PROTOCOL_VERSION};
use koshi_link::discovery::SessionRow;

#[test]
fn list_sessions_lists_an_unsaved_server_and_saves_it_holding_the_fingerprint_given() {
    let remote_session_rows = vec![RemoteSessionRow {
        session_id: SessionId::new(),
        session_name: "quiet-lake".to_string(),
    }];
    let (server_address, certificate_fingerprint) = spawn_stand_in_remote_server(vec![
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        },
        RemoteServerFrame::Sessions {
            session_rows: remote_session_rows.clone(),
        },
    ]);
    let home_directory = build_short_test_directory();

    let listing_output = build_koshi_command_under_home(home_directory.path())
        .args([
            "list-sessions",
            "--remote",
            &server_address,
            "--certificate-fingerprint",
            &certificate_fingerprint,
        ])
        .env("KOSHI_REMOTE_SECRET", "test-secret")
        .output()
        .expect("the koshi binary runs");

    let expected_session_rows: Vec<SessionRow> = remote_session_rows
        .iter()
        .map(|remote_session_row| {
            SessionRow::from_session(
                remote_session_row.session_id,
                &remote_session_row.session_name,
                Some(server_address.clone()),
            )
        })
        .collect();
    assert_eq!(String::from_utf8_lossy(&listing_output.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&listing_output.stdout),
        koshi::output::render_sessions(&expected_session_rows, koshi::cli::OutputFormat::Table)
    );
    assert_eq!(
        listing_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(None, server_address, Some(certificate_fingerprint))]
    );
}

#[test]
fn an_action_verb_reaches_an_unsaved_server_and_saves_it_holding_the_fingerprint_given() {
    // The stand-in server lists no session: `new-pane` finds no session to
    // open a pane in.
    let (server_address, certificate_fingerprint) = spawn_stand_in_remote_server(vec![
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        },
        RemoteServerFrame::Sessions {
            session_rows: Vec::new(),
        },
    ]);
    let home_directory = build_short_test_directory();

    let verb_output = build_koshi_command_under_home(home_directory.path())
        .args([
            "new-pane",
            "--remote",
            &server_address,
            "--certificate-fingerprint",
            &certificate_fingerprint,
        ])
        .env("KOSHI_REMOTE_SECRET", "test-secret")
        .output()
        .expect("the koshi binary runs");

    assert_eq!(
        String::from_utf8_lossy(&verb_output.stderr),
        "koshi: no koshi session is running\n"
    );
    assert_eq!(String::from_utf8_lossy(&verb_output.stdout), "");
    assert_eq!(
        verb_output.status.code(),
        Some(CliExitCode::SessionNotFound.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(None, server_address, Some(certificate_fingerprint))]
    );
}

#[test]
fn a_session_in_this_machines_shared_directory_is_refused_with_the_remote_flag() {
    let owned_session_text = format!("1001/{}", SessionId::new());
    let home_directory = build_short_test_directory();
    let refused_argument_cases: [(Vec<&str>, String); 3] = [
        (
            vec!["attach", "--remote", "some-other-box", &owned_session_text],
            format!(
                "koshi: invalid arguments: --remote takes a session id or name, not \
                 `{owned_session_text}`, which names a folder of this machine's shared directory\n"
            ),
        ),
        (
            vec![
                "close-tab",
                "--remote",
                "some-other-box",
                "--session",
                &owned_session_text,
            ],
            format!(
                "koshi: invalid arguments: --remote takes a session id or name, not \
                 `{owned_session_text}`, which names a folder of this machine's shared directory\n"
            ),
        ),
        (
            vec!["attach", "--remote", "some-other-box", "1001/quiet-lake"],
            "koshi: invalid arguments: expected `<user id>/session-<uuid>`, such as \
             `1001/session-<uuid>`\n"
                .to_string(),
        ),
    ];

    for (command_arguments, expected_stderr) in refused_argument_cases {
        let refused_output = build_koshi_command_under_home(home_directory.path())
            .args(&command_arguments)
            .output()
            .expect("the koshi binary runs");

        assert_eq!(
            String::from_utf8_lossy(&refused_output.stderr),
            expected_stderr,
            "{command_arguments:?}"
        );
        assert_eq!(String::from_utf8_lossy(&refused_output.stdout), "");
        assert_eq!(
            refused_output.status.code(),
            Some(CliExitCode::UsageOrConfig.get_exit_code()),
            "{command_arguments:?}"
        );
        assert_eq!(list_saved_server_records(home_directory.path()), Vec::new());
    }
}
