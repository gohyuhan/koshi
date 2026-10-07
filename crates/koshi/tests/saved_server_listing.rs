//! `koshi list-sessions` over the servers this machine saved.
//!
//! Each test starts the real `koshi` binary under a temporary home directory
//! and writes the saved-server store that binary reads. No connection leaves
//! this machine: a server with no pinned certificate is not dialled, and every
//! dialled server is a loopback port, either one that nothing listens on or a
//! TLS server that the test runs.

mod common;

use std::net::TcpListener;
use std::process::Output;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use common::{
    build_koshi_command_under_home, build_short_test_directory, resolve_data_directory_under_home,
};
use koshi_core::command::CliExitCode;
use koshi_core::ids::SessionId;
use koshi_ipc::protocol::ConnectionToken;
use koshi_ipc::remote_servers::{resolve_server_store_path, SavedServer, ServerStore};
use koshi_ipc::remote_wire::{
    RemoteClientFrame, RemoteServerFrame, RemoteSessionRow, REMOTE_PROTOCOL_VERSION, REMOTE_REFUSED,
};
use koshi_ipc::tls;
use koshi_ipc::transport::build_frame_halves;
use koshi_link::discovery::SessionRow;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection};

/// A saved server named `server_name` at `server_address`, pinning
/// `certificate_fingerprint`, saved at timestamp `0` and never used.
fn build_saved_server(
    server_name: &str,
    server_address: String,
    certificate_fingerprint: Option<String>,
) -> SavedServer {
    SavedServer {
        server_name: Some(server_name.to_string()),
        server_address,
        connection_token: ConnectionToken::from_secret("test-secret"),
        certificate_fingerprint,
        added_at: SystemTime::UNIX_EPOCH,
        last_used_at: None,
    }
}

/// Run `koshi list-sessions` under a new temporary home directory whose
/// saved-server store holds `saved_servers`, and return what it printed.
fn run_list_sessions_with_saved_servers(saved_servers: Vec<SavedServer>) -> Output {
    let home_directory = build_short_test_directory();
    let server_store = ServerStore {
        saved_servers,
        ..ServerStore::new()
    };
    server_store
        .write_server_store_to_path(&resolve_server_store_path(
            &resolve_data_directory_under_home(home_directory.path()),
        ))
        .expect("the saved-server store is written");
    build_koshi_command_under_home(home_directory.path())
        .arg("list-sessions")
        .output()
        .expect("the koshi binary runs")
}

/// Start a TLS server on a loopback port that presents a certificate made for
/// this call, and serve one connection on a thread of its own. For each frame
/// of `answer_frames`, in order, the thread reads one client frame, then sends
/// that answer frame. The thread ends after the last answer, or at the first
/// handshake, read or write that fails.
///
/// Returns the server's address and the fingerprint of its certificate.
fn spawn_stand_in_remote_server(answer_frames: Vec<RemoteServerFrame>) -> (String, String) {
    let generated_certificate = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .expect("the test certificate is generated");
    let certificate_der_bytes = generated_certificate.cert.der().to_vec();
    let certificate_fingerprint = tls::compute_certificate_fingerprint(&certificate_der_bytes);
    let server_config = ServerConfig::builder_with_provider(tls::build_crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports every default protocol version")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate_der_bytes)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                generated_certificate.signing_key.serialize_der(),
            )),
        )
        .expect("the test certificate is usable");
    let server_listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let server_address = server_listener
        .local_addr()
        .expect("the bound loopback address")
        .to_string();

    std::thread::spawn(move || {
        let Ok((mut socket, _)) = server_listener.accept() else {
            return;
        };
        let mut tls_connection = rustls::Connection::Server(
            ServerConnection::new(Arc::new(server_config)).expect("the TLS server side starts"),
        );
        let handshake_deadline = Instant::now() + common::WAIT_DURATION;
        if tls::run_tls_handshake(&mut tls_connection, &mut socket, handshake_deadline).is_err() {
            return;
        }
        let (tls_reader, tls_writer) =
            tls::split_tls_stream(tls_connection, socket).expect("the TLS stream splits");
        let (mut frame_reader, mut frame_writer) =
            build_frame_halves(Box::new(tls_reader), Box::new(tls_writer));
        for answer_frame in answer_frames {
            if frame_reader.recv::<RemoteClientFrame>().is_err()
                || frame_writer.send(&answer_frame).is_err()
            {
                return;
            }
        }
    });
    (server_address, certificate_fingerprint)
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
    let pinned_certificate_fingerprint = "0".repeat(64);

    let listing_output = run_list_sessions_with_saved_servers(vec![
        build_saved_server(
            "lab",
            closed_server_address,
            Some(pinned_certificate_fingerprint.clone()),
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
            Some(pinned_certificate_fingerprint.clone()),
        ),
    ]);

    assert_eq!(
        String::from_utf8_lossy(&listing_output.stderr),
        format!(
            "koshi: desk has no pinned certificate yet; run `koshi list-sessions --remote desk` \
             to connect and pin it\n\
             koshi: gate: the server {refusing_server_address} did not admit the connection. if \
             that machine runs koshi 0.3.0 or 0.4.0, update koshi there. otherwise the token was \
             rejected or revoked: re-grant it on that machine with `koshi share grant`, then \
             store the new secret with `koshi remote set-secret` for a saved server, or give it \
             when the next dial asks\n\
             koshi: lab did not answer; its sessions are not listed\n\
             koshi: mill: the certificate of {reinstalled_server_address} changed: pinned \
             {pinned_certificate_fingerprint}, presented \
             {reinstalled_server_certificate_fingerprint}. if the server was reinstalled on \
             purpose, run `koshi remote forget {reinstalled_server_address}` and connect again. \
             its sessions are not listed\n"
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
            "koshi: server-{server_number:02} has no pinned certificate yet; run `koshi \
             list-sessions --remote server-{server_number:02}` to connect and pin it\n"
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
