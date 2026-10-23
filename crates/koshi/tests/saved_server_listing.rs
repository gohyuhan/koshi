//! `koshi list-sessions` over the servers this machine saved.
//!
//! Each test starts the real `koshi` binary under a temporary home directory
//! and writes the saved-server store that binary reads. No connection leaves
//! this machine: a server holding no certificate fingerprint is not dialled,
//! and every dialled server is a loopback port, either one that nothing
//! listens on or a TLS server that the test runs.

mod common;

use std::net::TcpListener;
use std::process::Output;

use common::{
    build_koshi_command_under_home, build_saved_server, build_short_test_directory,
    spawn_stand_in_remote_server, write_saved_server_store,
};
use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
use koshi_ipc::remote_servers::SavedServer;
use koshi_ipc::remote_wire::{
    RemoteServerFrame, RemoteSessionRow, REMOTE_PROTOCOL_VERSION, REMOTE_REFUSED,
};
use koshi_link::discovery::SessionRow;

/// Run `koshi list-sessions` under a new temporary home directory whose
/// saved-server store holds `saved_servers`, and return what it printed.
fn run_list_sessions_with_saved_servers(saved_servers: Vec<SavedServer>) -> Output {
    let home_directory = build_short_test_directory();
    write_saved_server_store(home_directory.path(), saved_servers);
    build_koshi_command_under_home(home_directory.path())
        .arg("list-sessions")
        .output()
        .expect("the koshi binary runs")
}

#[test]
fn list_sessions_names_each_saved_server_it_cannot_list_on_standard_error() {
    let closed_port_listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let closed_server_address = closed_port_listener
        .local_addr()
        .expect("the bound loopback address")
        .to_string();
    drop(closed_port_listener);
    let (refusing_server_address, refusing_server_certificate_fingerprint) =
        spawn_stand_in_remote_server(vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        }]);
    let (reinstalled_server_address, reinstalled_server_certificate_fingerprint) =
        spawn_stand_in_remote_server(Vec::new());
    let saved_certificate_fingerprint = "0".repeat(64);

    let listing_output = run_list_sessions_with_saved_servers(vec![
        build_saved_server(
            "lab",
            closed_server_address,
            Some(saved_certificate_fingerprint.clone()),
        ),
        build_saved_server("desk", "127.0.0.1:1".to_string(), None),
        build_saved_server(
            "gate",
            refusing_server_address.clone(),
            Some(refusing_server_certificate_fingerprint),
        ),
        build_saved_server(
            "mill",
            reinstalled_server_address.clone(),
            Some(saved_certificate_fingerprint.clone()),
        ),
    ]);

    assert_eq!(
        String::from_utf8_lossy(&listing_output.stderr),
        format!(
            "koshi: desk holds no certificate fingerprint, so it is not dialled; run `koshi \
             remote edit desk` and give the fingerprint `koshi share grant` printed on that \
             machine\n\
             koshi: gate: the server {refusing_server_address} did not admit the connection. if \
             that machine runs koshi 0.3.0 or 0.4.0, update koshi there. otherwise the token was \
             rejected or revoked: re-grant it on that machine with `koshi share grant`, then \
             store the new secret with `koshi remote set-secret` for a saved server, or give it \
             when the next dial asks\n\
             koshi: lab did not answer; its sessions are not listed\n\
             koshi: mill: the certificate {reinstalled_server_address} presented is not the \
             expected one: expected {saved_certificate_fingerprint}, presented \
             {reinstalled_server_certificate_fingerprint}. the secret was not sent. if that \
             machine's certificate changed on purpose, `koshi share open` there prints the new \
             fingerprint: give it with --certificate-fingerprint, or with `koshi remote edit` for \
             a saved server. its sessions are not listed\n"
        )
    );
    assert_eq!(
        String::from_utf8_lossy(&listing_output.stdout),
        koshi::output::render_sessions(&[], koshi::cli::OutputFormat::Table)
    );
    assert_eq!(
        listing_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
}

#[test]
fn list_sessions_lists_each_session_a_saved_server_answers_with_beside_its_name() {
    let remote_session_rows = vec![
        RemoteSessionRow {
            session_id: SessionId::new(),
            session_name: "quiet-lake".to_string(),
        },
        RemoteSessionRow {
            session_id: SessionId::new(),
            session_name: "bright-fern".to_string(),
        },
    ];
    let (answering_server_address, answering_server_certificate_fingerprint) =
        spawn_stand_in_remote_server(vec![
            RemoteServerFrame::Welcome {
                remote_protocol_version: REMOTE_PROTOCOL_VERSION,
            },
            RemoteServerFrame::Sessions {
                session_rows: remote_session_rows.clone(),
            },
        ]);

    let listing_output = run_list_sessions_with_saved_servers(vec![build_saved_server(
        "work",
        answering_server_address,
        Some(answering_server_certificate_fingerprint),
    )]);

    let expected_session_rows: Vec<SessionRow> = remote_session_rows
        .iter()
        .map(|remote_session_row| {
            SessionRow::from_session(
                remote_session_row.session_id,
                &remote_session_row.session_name,
                Some("work".to_string()),
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
}

#[test]
fn list_sessions_asks_the_first_16_saved_servers_and_names_how_many_are_saved() {
    let saved_servers: Vec<SavedServer> = (1..=17)
        .map(|server_number| {
            build_saved_server(
                &format!("server-{server_number:02}"),
                format!("127.0.0.1:{server_number}"),
                None,
            )
        })
        .collect();

    let listing_output = run_list_sessions_with_saved_servers(saved_servers);

    let mut expected_error_output = "koshi: asking the first 16 of 17 saved servers; name one \
                                     with `--remote <server>` to reach the rest\n"
        .to_string();
    for server_number in 1..=16 {
        expected_error_output.push_str(&format!(
            "koshi: server-{server_number:02} holds no certificate fingerprint, so it is not \
             dialled; run `koshi remote edit server-{server_number:02}` and give the fingerprint \
             `koshi share grant` printed on that machine\n"
        ));
    }
    assert_eq!(
        String::from_utf8_lossy(&listing_output.stderr),
        expected_error_output
    );
    assert_eq!(
        String::from_utf8_lossy(&listing_output.stdout),
        koshi::output::render_sessions(&[], koshi::cli::OutputFormat::Table)
    );
    assert_eq!(
        listing_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
}
