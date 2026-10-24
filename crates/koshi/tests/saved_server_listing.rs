//! `koshi list-sessions` over the servers this machine saved.
//!
//! Each test starts the real `koshi` binary under a temporary home directory
//! and writes the saved-server store that binary reads. No connection leaves
//! this machine: a server holding no certificate fingerprint is not dialled,
//! and every dialled server is a loopback port the test holds: one that closes
//! each connection it accepts, one that never accepts, or a TLS server that
//! the test runs.

mod common;

use std::net::TcpListener;
use std::process::Output;

use common::{
    build_koshi_command_under_home, build_saved_server, build_short_test_directory,
    list_saved_server_records, resolve_data_directory_under_home, spawn_stand_in_remote_server,
    write_saved_server_store,
};
use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
use koshi_ipc::remote_servers::{resolve_server_store_path, SavedServer};
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
    let closing_listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let closing_server_address = closing_listener
        .local_addr()
        .expect("the bound loopback address")
        .to_string();
    std::thread::spawn(move || {
        // Each connection is accepted and closed before a byte is answered.
        for accepted_connection in closing_listener.incoming() {
            drop(accepted_connection);
        }
    });
    // Bound and never accepted: the dial waits in its TLS handshake until the
    // listing stops waiting.
    let silent_listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let silent_server_address = silent_listener
        .local_addr()
        .expect("the bound loopback address")
        .to_string();
    let (refusing_server_address, refusing_server_certificate_fingerprint) =
        spawn_stand_in_remote_server(vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
            remote_protocol_version: None,
        }]);
    let (reinstalled_server_address, reinstalled_server_certificate_fingerprint) =
        spawn_stand_in_remote_server(Vec::new());
    let saved_certificate_fingerprint = "0".repeat(64);

    let listing_output = run_list_sessions_with_saved_servers(vec![
        build_saved_server(
            "lab",
            closing_server_address.clone(),
            Some(saved_certificate_fingerprint.clone()),
        ),
        build_saved_server(
            "slow",
            silent_server_address,
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
             koshi: gate: the server {refusing_server_address} did not admit the connection: \
             the token expired, was revoked, or was never granted on that machine, that machine \
             already serves as many remote connections as it admits, or it runs koshi 0.3.0 or \
             0.4.0, which cannot read this koshi's request. `koshi share list` on that machine \
             shows when each grant expires or was revoked. to replace the token, run `koshi \
             share grant` there, then store the new secret with `koshi remote set-secret gate`. \
             if that machine runs koshi 0.3.0 or 0.4.0, update koshi there; its sessions are not \
             listed\n\
             koshi: lab: IPC unavailable: the connection to {closing_server_address} closed \
             during the TLS handshake. a koshi server closes a connection this way when it \
             restarts or stops, when 64 others wait to present a secret, and when the address it \
             comes from opened 10 that were not admitted in 60 seconds. dial again in 60 seconds. \
             if it keeps happening, run `koshi doctor` on that machine to check that koshi serves \
             that port; its sessions are not listed\n\
             koshi: mill: IPC unavailable: the certificate {reinstalled_server_address} presented \
             is not the expected one: expected sha256:{saved_certificate_fingerprint}, presented \
             sha256:{reinstalled_server_certificate_fingerprint}. the secret was not sent. if that \
             machine's certificate changed on purpose, `koshi share open` there prints the new \
             fingerprint: give it with `koshi remote edit mill`; its sessions are not listed\n\
             koshi: slow did not answer; its sessions are not listed\n"
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

#[test]
fn a_saved_server_file_the_previous_build_wrote_loads_lists_sha256_and_dials_unchanged() {
    let remote_session_row = RemoteSessionRow {
        session_id: SessionId::new(),
        session_name: "quiet-lake".to_string(),
    };
    let (answering_server_address, answering_server_certificate_fingerprint) =
        spawn_stand_in_remote_server(vec![
            RemoteServerFrame::Welcome {
                remote_protocol_version: REMOTE_PROTOCOL_VERSION,
            },
            RemoteServerFrame::Sessions {
                session_rows: vec![remote_session_row.clone()],
            },
        ]);
    let home_directory = build_short_test_directory();
    write_saved_server_store(home_directory.path(), Vec::new());
    std::fs::write(
        resolve_server_store_path(&resolve_data_directory_under_home(home_directory.path())),
        format!(
            r#"{{"store_format":2,"saved_servers":[{{"server_name":"work","server_address":"{answering_server_address}","connection_token":"test-secret","certificate_fingerprint":"{answering_server_certificate_fingerprint}","added_at":{{"secs_since_epoch":0,"nanos_since_epoch":0}},"last_used_at":null}}]}}"#
        ),
    )
    .expect("the saved-server file of the previous build is written");

    let remote_list_output = build_koshi_command_under_home(home_directory.path())
        .args(["remote", "list", "--format", "json"])
        .output()
        .expect("the koshi binary runs");
    assert_eq!(String::from_utf8_lossy(&remote_list_output.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&remote_list_output.stdout),
        format!(
            r#"[
  {{
    "server_name": "work",
    "server_address": "{answering_server_address}",
    "certificate_fingerprint": "sha256:{answering_server_certificate_fingerprint}",
    "last_used_at": null
  }}
]
"#
        )
    );

    let listing_output = build_koshi_command_under_home(home_directory.path())
        .arg("list-sessions")
        .output()
        .expect("the koshi binary runs");
    assert_eq!(String::from_utf8_lossy(&listing_output.stderr), "");
    assert_eq!(
        String::from_utf8_lossy(&listing_output.stdout),
        koshi::output::render_sessions(
            &[SessionRow::from_session(
                remote_session_row.session_id,
                &remote_session_row.session_name,
                Some("work".to_string()),
            )],
            koshi::cli::OutputFormat::Table
        )
    );
    assert_eq!(
        listing_output.status.code(),
        Some(CliExitCode::Success.get_exit_code())
    );
    assert_eq!(
        list_saved_server_records(home_directory.path()),
        vec![(
            Some("work".to_string()),
            answering_server_address,
            Some(answering_server_certificate_fingerprint)
        )]
    );
}
