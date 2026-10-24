//! `KOSHI_REMOTE_SECRET` beside a server this machine saved.
//!
//! Each test starts the real `koshi` binary under a temporary home directory
//! and writes the saved-server store that binary reads. A `--remote` verb
//! naming a saved server refuses a value that no saved server holds, before
//! any dial. A value a saved server holds is dialled with the secret the
//! named server holds. A saved server that does not admit its secret is
//! named in the `koshi remote set-secret` step. The sweep of a bare
//! `koshi list-sessions` and the session a bare `koshi attach` picks do not
//! read the variable. No
//! connection leaves this machine: every dialled server is a loopback port,
//! either one that nothing listens on or a TLS server that the test runs.

mod common;

use std::io::Write;
use std::process::Stdio;

use common::{
    build_koshi_command_under_home, build_saved_server, build_short_test_directory,
    list_saved_server_records, spawn_scripted_remote_server, spawn_stand_in_remote_server,
    write_saved_server_store,
};
use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_servers::SavedServer;
use koshi_ipc::remote_wire::{
    RemoteServerFrame, RemoteSessionRow, REMOTE_PROTOCOL_VERSION, REMOTE_REFUSED,
};
use koshi_link::discovery::SessionRow;

/// The answers of a server that admits the secret and lists `session_rows`.
fn build_listing_answer_frames(session_rows: Vec<RemoteSessionRow>) -> Vec<RemoteServerFrame> {
    vec![
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        },
        RemoteServerFrame::Sessions { session_rows },
    ]
}

/// The table `koshi list-sessions` prints for `remote_session_rows`, each
/// beside the server `work`.
fn render_work_session_table(remote_session_rows: &[RemoteSessionRow]) -> String {
    let session_rows: Vec<SessionRow> = remote_session_rows
        .iter()
        .map(|remote_session_row| {
            SessionRow::from_session(
                remote_session_row.session_id,
                &remote_session_row.session_name,
                Some("work".to_string()),
            )
        })
        .collect();
    koshi::output::render_sessions(&session_rows, koshi::cli::OutputFormat::Table)
}

#[test]
fn a_remote_verb_refuses_a_secret_variable_no_saved_server_holds_before_any_dial() {
    // Nothing listens on the address: a dial would end in a refusal of its own.
    let server_address = "127.0.0.1:1".to_string();
    let saved_certificate_fingerprint = "0".repeat(64);
    let saved_certificate_fingerprint_text = format!("sha256:{saved_certificate_fingerprint}");
    let refused_command_cases: [Vec<&str>; 4] = [
        vec!["attach", "--remote", "work"],
        vec!["list-sessions", "--remote", "work"],
        vec!["new-pane", "--remote", "work"],
        vec![
            "attach",
            "--remote",
            &server_address,
            "--certificate-fingerprint",
            &saved_certificate_fingerprint_text,
            "--save-as",
            "work",
        ],
    ];

    for command_arguments in refused_command_cases {
        let home_directory = build_short_test_directory();
        write_saved_server_store(
            home_directory.path(),
            vec![build_saved_server(
                "work",
                server_address.clone(),
                Some(saved_certificate_fingerprint.clone()),
            )],
        );

        let refused_output = build_koshi_command_under_home(home_directory.path())
            .args(&command_arguments)
            .env("KOSHI_REMOTE_SECRET", "fresh-secret")
            .output()
            .expect("the koshi binary runs");

        assert_eq!(
            String::from_utf8_lossy(&refused_output.stderr),
            "koshi: invalid arguments: work holds a saved secret, and KOSHI_REMOTE_SECRET gives \
             one that no saved server holds; run `koshi remote set-secret work` to replace the \
             saved secret with it, or unset KOSHI_REMOTE_SECRET to dial with the saved one\n",
            "{command_arguments:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&refused_output.stdout),
            "",
            "{command_arguments:?}"
        );
        assert_eq!(
            refused_output.status.code(),
            Some(CliExitCode::UsageOrConfig.get_exit_code()),
            "{command_arguments:?}"
        );
        assert_eq!(
            list_saved_server_records(home_directory.path()),
            vec![(
                Some("work".to_string()),
                server_address.clone(),
                Some(saved_certificate_fingerprint.clone())
            )],
            "{command_arguments:?}"
        );
    }
}

#[test]
fn a_remote_verb_dials_with_the_named_servers_secret_when_the_variable_holds_a_saved_one() {
    let remote_session_rows = vec![RemoteSessionRow {
        session_id: SessionId::new(),
        session_name: "quiet-lake".to_string(),
    }];
    let (server_address, certificate_fingerprint) =
        spawn_stand_in_remote_server(build_listing_answer_frames(remote_session_rows.clone()));
    let saved_servers = vec![
        build_saved_server("work", server_address, Some(certificate_fingerprint)),
        SavedServer {
            connection_token: ConnectionToken::from_secret("desk-secret"),
            ..build_saved_server("desk", "127.0.0.1:1".to_string(), Some("0".repeat(64)))
        },
    ];

    // `test-secret` is the secret `work` holds; `desk-secret` is the one
    // `desk` holds.
    for environment_secret in ["test-secret", "desk-secret"] {
        let home_directory = build_short_test_directory();
        write_saved_server_store(home_directory.path(), saved_servers.clone());

        let listing_output = build_koshi_command_under_home(home_directory.path())
            .args(["list-sessions", "--remote", "work"])
            .env("KOSHI_REMOTE_SECRET", environment_secret)
            .output()
            .expect("the koshi binary runs");

        assert_eq!(
            String::from_utf8_lossy(&listing_output.stderr),
            "",
            "{environment_secret}"
        );
        assert_eq!(
            String::from_utf8_lossy(&listing_output.stdout),
            render_work_session_table(&remote_session_rows),
            "{environment_secret}"
        );
        assert_eq!(
            listing_output.status.code(),
            Some(CliExitCode::Success.get_exit_code()),
            "{environment_secret}"
        );
    }
}

#[test]
fn a_saved_server_that_refuses_its_secret_names_set_secret_with_its_name() {
    let (server_address, certificate_fingerprint) =
        spawn_stand_in_remote_server(vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        }]);
    let home_directory = build_short_test_directory();
    write_saved_server_store(
        home_directory.path(),
        vec![build_saved_server(
            "work",
            server_address.clone(),
            Some(certificate_fingerprint),
        )],
    );

    let listing_output = build_koshi_command_under_home(home_directory.path())
        .args(["list-sessions", "--remote", "work"])
        .output()
        .expect("the koshi binary runs");

    assert_eq!(
        String::from_utf8_lossy(&listing_output.stderr),
        format!(
            "koshi: the server {server_address} did not admit the connection. if that machine \
             runs koshi 0.3.0 or 0.4.0, update koshi there. otherwise the token was rejected or \
             revoked: re-grant it on that machine with `koshi share grant`, then store the new \
             secret with `koshi remote set-secret work`\n"
        )
    );
    assert_eq!(String::from_utf8_lossy(&listing_output.stdout), "");
    assert_eq!(
        listing_output.status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
}

#[test]
fn a_bare_listing_presents_each_saved_secret_and_does_not_read_the_secret_variable() {
    let remote_session_rows = vec![RemoteSessionRow {
        session_id: SessionId::new(),
        session_name: "quiet-lake".to_string(),
    }];
    let (server_address, certificate_fingerprint) =
        spawn_stand_in_remote_server(build_listing_answer_frames(remote_session_rows.clone()));
    let home_directory = build_short_test_directory();
    write_saved_server_store(
        home_directory.path(),
        vec![build_saved_server(
            "work",
            server_address,
            Some(certificate_fingerprint),
        )],
    );

    let listing_output = build_koshi_command_under_home(home_directory.path())
        .arg("list-sessions")
        .env("KOSHI_REMOTE_SECRET", "fresh-secret")
        .output()
        .expect("the koshi binary runs");

    assert_eq!(String::from_utf8_lossy(&listing_output.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&listing_output.stdout),
        render_work_session_table(&remote_session_rows)
    );
    assert_eq!(
        listing_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
}

#[test]
fn a_session_picked_from_a_bare_attach_is_dialled_without_reading_the_secret_variable() {
    let session_id = SessionId::new();
    // The first connection is the sweep, which lists one session. The second
    // is the join of the picked session: the server admits the secret, then
    // refuses the session.
    let (server_address, certificate_fingerprint) = spawn_scripted_remote_server(
        vec![
            build_listing_answer_frames(vec![RemoteSessionRow {
                session_id,
                session_name: "quiet-lake".to_string(),
            }]),
            vec![
                RemoteServerFrame::Welcome {
                    remote_protocol_version: REMOTE_PROTOCOL_VERSION,
                },
                RemoteServerFrame::Refused {
                    message: "that session is not granted".to_string(),
                },
            ],
        ]
        .into_iter(),
    );
    let home_directory = build_short_test_directory();
    write_saved_server_store(
        home_directory.path(),
        vec![build_saved_server(
            "work",
            server_address,
            Some(certificate_fingerprint),
        )],
    );

    let mut attach_process = build_koshi_command_under_home(home_directory.path())
        .arg("attach")
        .env("KOSHI_REMOTE_SECRET", "fresh-secret")
        .stdin(Stdio::piped())
        .spawn()
        .expect("the koshi binary starts");
    attach_process
        .stdin
        .take()
        .expect("the attach reads a pipe")
        .write_all(b"1\n")
        .expect("the answer is written");
    let attach_output = attach_process
        .wait_with_output()
        .expect("the koshi binary ends");

    assert_eq!(
        String::from_utf8_lossy(&attach_output.stderr),
        format!("koshi: the token this server saved does not reach session {session_id}\n")
    );
    assert_eq!(
        String::from_utf8_lossy(&attach_output.stdout),
        format!("1) quiet-lake {session_id} (remote: work)\nattach to which session? [1] ")
    );
    assert_eq!(
        attach_output.status.code(),
        Some(CliExitCode::RuntimeAction.get_exit_code())
    );
}
