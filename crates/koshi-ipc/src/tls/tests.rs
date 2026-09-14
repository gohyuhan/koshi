//! Tests for the TLS stream: the certificate fingerprint, what the pinning
//! verifier accepts and refuses, the key exchange groups the provider offers
//! and the one a handshake settles on, the frames that cross a real loopback
//! stream, the cause a failed dial carries and the words it prints, the
//! deadline a handshake, an opening exchange, a read and a write finish
//! inside, against a peer that answers nothing and against one that sends a
//! byte at a time, how a read ends when the peer closes the stream, cuts it
//! or sends bytes that do not decrypt, how much of one write is taken, and
//! the socket timeouts a deadline sets and takes away.
//!
//! The loopback tests bind `127.0.0.1:0`, so the operating system picks a free
//! port and two runs of the suite never meet on one address.

use std::io;
use std::net::TcpListener;

use koshi_core::ids::SessionId;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::{NamedGroup, ServerConfig, ServerConnection};

use super::*;
use crate::protocol::ConnectionToken;
use crate::remote_wire::{
    open_remote_connection, RemoteClientFrame, RemoteServerFrame, RemoteSessionRow,
    MIN_REMOTE_PROTOCOL_VERSION, REMOTE_PROTOCOL_VERSION,
};
use crate::transport::{frame_halves, Deadlined};

/// How long a loopback handshake and the frames after it have to finish. Well
/// past what a loopback stream needs, so a slow machine does not fail the run.
const LOOPBACK_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

/// The timeout the deadline tests give a dial.
const SHORT_TIMEOUT_DURATION: Duration = Duration::from_millis(300);

/// How far past its timeout a dial may still return, so a busy machine that
/// takes a moment to schedule the returning thread does not fail the run. The
/// bound is what the test proves: the dial returns, rather than waiting on a
/// server that never answers.
const DEADLINE_SLACK_DURATION: Duration = Duration::from_secs(3);

/// The timeout the opening-exchange tests give a dial: room for a loopback
/// handshake on a busy machine, and far less than the send_test_drip_bytes and the pause that
/// follow it.
const OPENING_TIMEOUT_DURATION: Duration = Duration::from_secs(1);

/// How long a server waits before the frame it sends after the answer. Past
/// [`OPENING_TIMEOUT_DURATION`], so a read still holding the dial's deadline would fail.
const PAUSE_AFTER_OPENING_RESPONSE_DURATION: Duration = Duration::from_millis(1500);

/// How long the send_test_drip_bytes tests leave between the bytes they send.
const DRIP_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// How many bytes the send_test_drip_bytes tests send. At one byte every [`DRIP_INTERVAL_DURATION`] the send_test_drip_bytes
/// lasts far longer than [`SHORT_TIMEOUT_DURATION`] or [`OPENING_TIMEOUT_DURATION`] with [`DEADLINE_SLACK_DURATION`]
/// on top, so a peer that stretched its deadline by dripping would fail these
/// tests.
const DRIP_BYTE_COUNT: usize = 200;

/// The header of a TLS record of `tls_record_type`, the version, and a payload of 256
/// bytes. The drip helper that follows never reaches that many, so the TLS record is
/// never whole and the reader keeps wanting more.
fn build_test_tls_record_header(tls_record_type: u8) -> [u8; 5] {
    [tls_record_type, 0x03, 0x03, 0x01, 0x00]
}

/// Send `record_header` and then one byte every [`DRIP_INTERVAL_DURATION`], stopping early when the
/// peer has closed the socket.
fn send_test_drip_bytes(socket: &mut TcpStream, record_header: [u8; 5]) {
    if socket.write_all(&record_header).is_err() {
        return;
    }
    for _ in 0..DRIP_BYTE_COUNT {
        std::thread::sleep(DRIP_INTERVAL_DURATION);
        if socket.write_all(&[0]).is_err() {
            return;
        }
    }
}

/// The name a verifier is handed. Never checked: the fingerprint is what a
/// server is recognised by.
fn build_test_server_name() -> ServerName<'static> {
    ServerName::try_from("127.0.0.1".to_string()).expect("a loopback address is a server name")
}

/// Ask `pin_verifier` about `certificate_der_bytes`.
fn verify_test_certificate(
    pin_verifier: &PinVerifier,
    certificate_der_bytes: &[u8],
) -> Result<ServerCertVerified, rustls::Error> {
    pin_verifier.verify_server_cert(
        &CertificateDer::from(certificate_der_bytes.to_vec()),
        &[],
        &build_test_server_name(),
        &[],
        UnixTime::since_unix_epoch(Duration::from_secs(1_700_000_000)),
    )
}

#[test]
fn the_fingerprint_is_the_sha256_as_sixty_four_lowercase_hex_characters() {
    assert_eq!(
        compute_certificate_fingerprint(&[]),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn a_first_connection_takes_the_certificate_and_records_its_fingerprint() {
    let pin_verifier = PinVerifier::from_expected_certificate_fingerprint(None);
    assert_eq!(pin_verifier.get_presented_certificate_fingerprint(), None);
    verify_test_certificate(&pin_verifier, b"a certificate")
        .expect("a first connection takes any certificate");
    assert_eq!(
        pin_verifier.get_presented_certificate_fingerprint(),
        Some(compute_certificate_fingerprint(b"a certificate"))
    );
}

#[test]
fn the_pinned_fingerprint_is_taken_and_every_other_one_is_refused() {
    let pinned_certificate_fingerprint = compute_certificate_fingerprint(b"the first certificate");
    let pin_verifier =
        PinVerifier::from_expected_certificate_fingerprint(Some(&pinned_certificate_fingerprint));
    verify_test_certificate(&pin_verifier, b"the first certificate")
        .expect("the pinned certificate is taken");
    assert_eq!(
        pin_verifier.get_presented_certificate_fingerprint(),
        Some(pinned_certificate_fingerprint.clone())
    );

    let pin_verifier =
        PinVerifier::from_expected_certificate_fingerprint(Some(&pinned_certificate_fingerprint));
    let verification_error = verify_test_certificate(&pin_verifier, b"another certificate")
        .expect_err("a changed certificate");
    assert_eq!(
        verification_error,
        rustls::Error::General(format!(
            "the pinned certificate is {pinned_certificate_fingerprint}, the server presented {}",
            compute_certificate_fingerprint(b"another certificate")
        ))
    );
    assert_eq!(
        pin_verifier.get_presented_certificate_fingerprint(),
        Some(compute_certificate_fingerprint(b"another certificate"))
    );
}

#[test]
fn a_port_nothing_listens_on_refuses_the_dial_and_names_the_way_to_open_it() {
    // The operating system picks a free port. The listener goes before the
    // dial, and nothing holds that port when the connection arrives.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");
    drop(listener);

    let ipc_error =
        connect_tls_stream(&server_address.to_string(), None, LOOPBACK_TIMEOUT_DURATION)
            .expect_err("a port nothing listens on refuses the connection");

    let rendered_error = ipc_error.to_string();
    let IpcError::ConnectRefused {
        server_address: named_server_address,
    } = ipc_error
    else {
        panic!("a refused connection is its own failure, not a transport failure");
    };
    assert_eq!(named_server_address, server_address.to_string());
    assert_eq!(
        rendered_error,
        format!(
            "{server_address} refused the connection: nothing is listening on that port. \
             if remote access is not enabled on that machine, run `koshi share grant` \
             there and answer yes to the offer to open the port"
        )
    );
}

#[test]
fn a_dial_with_no_time_left_times_out_before_it_connects() {
    // A zero timeout leaves no time after the name lookup. The dial ends
    // before it opens a socket, and no bytes reach the address.
    let server_address = "127.0.0.1:1";

    let ipc_error = connect_tls_stream(server_address, None, Duration::ZERO)
        .expect_err("a dial with no time left never connects");

    let rendered_error = ipc_error.to_string();
    let IpcError::ConnectTimedOut {
        server_address: named_server_address,
    } = ipc_error
    else {
        panic!("a connect with no time left is a timeout, not a transport failure");
    };
    assert_eq!(named_server_address, server_address);
    assert_eq!(
        rendered_error,
        "connecting to 127.0.0.1:1 timed out: nothing answered. check that the machine is up, \
         the address and port are right, and the network path allows it"
    );
}

#[test]
fn a_failed_handshake_names_the_address_and_the_reason() {
    assert_eq!(
        IpcError::TlsHandshakeFailed {
            server_address: "laptop.local:7654".to_string(),
            error_detail: "the TLS handshake did not finish in time".to_string(),
        }
        .to_string(),
        "the TLS handshake with laptop.local:7654 failed: \
         the TLS handshake did not finish in time"
    );
}

#[test]
fn a_server_that_answers_nothing_ends_the_dial_at_the_deadline() {
    // Bound and never accepted: the operating system's backlog completes the
    // TCP connection, so the dial reaches the handshake and waits there.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let handshake_started_at = Instant::now();
    let ipc_error = connect_tls_stream(&server_address.to_string(), None, SHORT_TIMEOUT_DURATION)
        .expect_err("a server that sends nothing never finishes the handshake");
    let handshake_elapsed_duration = handshake_started_at.elapsed();

    let IpcError::TlsHandshakeFailed {
        server_address: named_server_address,
        error_detail,
    } = ipc_error
    else {
        panic!("a handshake that ran out of time is a handshake failure");
    };
    assert_eq!(named_server_address, server_address.to_string());
    assert_eq!(error_detail, "the TLS handshake did not finish in time");
    assert!(
        handshake_elapsed_duration < SHORT_TIMEOUT_DURATION + DEADLINE_SLACK_DURATION,
        "the dial returned {handshake_elapsed_duration:?} after it started, inside its {SHORT_TIMEOUT_DURATION:?} timeout"
    );
}

#[test]
fn a_server_that_sends_one_byte_at_a_time_ends_the_dial_at_the_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let server_thread = std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return;
        };
        send_test_drip_bytes(&mut socket, build_test_tls_record_header(0x16));
    });

    let handshake_started_at = Instant::now();
    let ipc_error = connect_tls_stream(&server_address.to_string(), None, SHORT_TIMEOUT_DURATION)
        .expect_err("a server that drips its bytes never finishes the handshake");
    let handshake_elapsed_duration = handshake_started_at.elapsed();

    let IpcError::TlsHandshakeFailed {
        server_address: named_server_address,
        error_detail,
    } = ipc_error
    else {
        panic!("a handshake that ran out of time is a handshake failure");
    };
    assert_eq!(named_server_address, server_address.to_string());
    assert_eq!(error_detail, "the TLS handshake did not finish in time");
    assert!(
        handshake_elapsed_duration < SHORT_TIMEOUT_DURATION + DEADLINE_SLACK_DURATION,
        "the dial returned {handshake_elapsed_duration:?} after it started, inside its {SHORT_TIMEOUT_DURATION:?} timeout, \
         though the server kept it fed with a byte every {DRIP_INTERVAL_DURATION:?}"
    );
    let _ = server_thread.join();
}

/// A fresh self-signed certificate and a server configuration serving it with
/// `crypto_provider`.
fn build_server_config_with_provider(
    crypto_provider: Arc<CryptoProvider>,
) -> (ServerConfig, Vec<u8>) {
    let generated_certificate = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .expect("generate a self-signed certificate");
    let certificate_der_bytes = generated_certificate.cert.der().to_vec();
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        generated_certificate.signing_key.serialize_der(),
    ));
    let server_config = ServerConfig::builder_with_provider(crypto_provider)
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports every default protocol version")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate_der_bytes.clone())],
            private_key,
        )
        .expect("a certificate and its key make a server configuration");
    (server_config, certificate_der_bytes)
}

/// A fresh self-signed certificate and the TLS configuration serving it, the
/// way this machine's own certificate is served.
fn build_fresh_server_config() -> (ServerConfig, Vec<u8>) {
    build_server_config_with_provider(build_crypto_provider())
}

/// A fresh self-signed certificate and a server configuration whose key
/// exchange list holds `X25519` alone.
fn build_classical_only_server_config() -> (ServerConfig, Vec<u8>) {
    let mut crypto_provider = rustls::crypto::aws_lc_rs::default_provider();
    crypto_provider.kx_groups = vec![rustls::crypto::aws_lc_rs::kx_group::X25519];
    build_server_config_with_provider(Arc::new(crypto_provider))
}

/// The one session a loopback server reports.
fn build_remote_session_row() -> RemoteSessionRow {
    RemoteSessionRow {
        session_id: SessionId::new(),
        session_name: "quiet-lake".to_string(),
    }
}

/// Serve `server_config` on a loopback port, dial it, and report the key exchange
/// group the handshake settled on together with the fingerprint the client
/// was shown.
fn negotiate_key_exchange_group(server_config: ServerConfig) -> (Option<NamedGroup>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let server_thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept the client");
        let server_connection =
            ServerConnection::new(Arc::new(server_config)).expect("a server connection");
        let mut tls_connection = rustls::Connection::Server(server_connection);
        run_tls_handshake(
            &mut tls_connection,
            &mut socket,
            Instant::now() + LOOPBACK_TIMEOUT_DURATION,
        )
        .expect("the loopback handshake finishes");
        tls_connection
            .negotiated_key_exchange_group()
            .map(|key_exchange_group| key_exchange_group.name())
    });

    let (_reader, _writer, presented_certificate_fingerprint) =
        connect_tls_stream(&server_address.to_string(), None, LOOPBACK_TIMEOUT_DURATION)
            .expect("the dial opens");
    let negotiated_key_exchange_group = server_thread.join().expect("the server thread finishes");
    (
        negotiated_key_exchange_group,
        presented_certificate_fingerprint,
    )
}

/// Two koshi peers settle on `X25519MLKEM768`: the X25519 elliptic curve
/// combined with ML-KEM-768, the post-quantum key encapsulation mechanism.
/// koshi offers that group ahead of every classical one.
#[test]
fn a_loopback_handshake_settles_on_the_hybrid_post_quantum_key_exchange() {
    let (server_config, certificate_der_bytes) = build_fresh_server_config();

    let (negotiated_key_exchange_group, presented_certificate_fingerprint) =
        negotiate_key_exchange_group(server_config);

    assert_eq!(
        negotiated_key_exchange_group,
        Some(NamedGroup::X25519MLKEM768)
    );
    assert_eq!(
        presented_certificate_fingerprint,
        compute_certificate_fingerprint(&certificate_der_bytes)
    );
}

/// A server whose key exchange list holds `X25519` alone accepts the dial,
/// and the handshake settles on `X25519` rather than failing.
#[test]
fn a_server_that_offers_only_classical_key_exchange_still_accepts_a_dial() {
    let (server_config, certificate_der_bytes) = build_classical_only_server_config();

    let (negotiated_key_exchange_group, presented_certificate_fingerprint) =
        negotiate_key_exchange_group(server_config);

    assert_eq!(negotiated_key_exchange_group, Some(NamedGroup::X25519));
    assert_eq!(
        presented_certificate_fingerprint,
        compute_certificate_fingerprint(&certificate_der_bytes)
    );
}

#[test]
fn frames_cross_a_loopback_stream_both_ways_and_the_client_pins_what_it_was_shown() {
    let (server_config, certificate_der_bytes) = build_fresh_server_config();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");
    let session_row = build_remote_session_row();
    let served_session_row = session_row.clone();

    let server_thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept the client");
        let server_connection =
            ServerConnection::new(Arc::new(server_config)).expect("a server connection");
        let mut tls_connection = rustls::Connection::Server(server_connection);
        run_tls_handshake(
            &mut tls_connection,
            &mut socket,
            Instant::now() + LOOPBACK_TIMEOUT_DURATION,
        )
        .expect("the loopback handshake finishes");
        let (reader, writer) =
            split_tls_stream(tls_connection, socket).expect("split the loopback stream");
        let (mut frame_reader, mut frame_writer) = frame_halves(Box::new(reader), Box::new(writer));

        let opening_frame: RemoteClientFrame =
            frame_reader.recv().expect("the client's opening frame");
        frame_writer
            .send(&RemoteServerFrame::Welcome {
                remote_protocol_version: REMOTE_PROTOCOL_VERSION,
            })
            .expect("answer the opening frame");
        let requested_frame: RemoteClientFrame =
            frame_reader.recv().expect("the client's next frame");
        frame_writer
            .send(&RemoteServerFrame::Sessions {
                session_rows: vec![served_session_row],
            })
            .expect("answer the list");
        (opening_frame, requested_frame)
    });

    let (reader, writer, presented_certificate_fingerprint) =
        connect_tls_stream(&server_address.to_string(), None, LOOPBACK_TIMEOUT_DURATION)
            .expect("the dial opens");
    assert_eq!(
        presented_certificate_fingerprint,
        compute_certificate_fingerprint(&certificate_der_bytes)
    );
    let (mut frame_reader, mut frame_writer) = frame_halves(Box::new(reader), Box::new(writer));

    let hello_frame = RemoteClientFrame::Hello {
        min_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        max_remote_version: REMOTE_PROTOCOL_VERSION,
        min_protocol_version: 1,
        max_protocol_version: 1,
        connection_token: ConnectionToken::from_secret("the secret the operator handed out"),
    };
    frame_writer
        .send(&hello_frame)
        .expect("send the opening frame");
    assert_eq!(
        frame_reader
            .recv::<RemoteServerFrame>()
            .expect("read the answer"),
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION
        }
    );
    frame_writer
        .send(&RemoteClientFrame::List)
        .expect("ask for the sessions");
    assert_eq!(
        frame_reader
            .recv::<RemoteServerFrame>()
            .expect("read the sessions"),
        RemoteServerFrame::Sessions {
            session_rows: vec![session_row],
        }
    );

    let (opening_frame, requested_frame) =
        server_thread.join().expect("the server thread finished");
    assert_eq!(opening_frame, hello_frame);
    assert_eq!(requested_frame, RemoteClientFrame::List);
}

#[test]
fn a_peer_that_drips_after_the_handshake_ends_a_read_at_the_readers_deadline() {
    let (server_config, _) = build_fresh_server_config();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let server_thread = std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return;
        };
        let Ok(server_connection) = ServerConnection::new(Arc::new(server_config)) else {
            return;
        };
        let mut tls_connection = rustls::Connection::Server(server_connection);
        if run_tls_handshake(
            &mut tls_connection,
            &mut socket,
            Instant::now() + LOOPBACK_TIMEOUT_DURATION,
        )
        .is_err()
        {
            return;
        }
        send_test_drip_bytes(&mut socket, build_test_tls_record_header(0x17));
    });

    let (mut reader, writer, _presented_certificate_fingerprint) =
        connect_tls_stream(&server_address.to_string(), None, LOOPBACK_TIMEOUT_DURATION)
            .expect("the dial opens");
    reader.set_deadline(Some(Instant::now() + SHORT_TIMEOUT_DURATION));

    let read_started_at = Instant::now();
    let mut frame_length_bytes = [0u8; 4];
    let read_error = reader
        .read_exact(&mut frame_length_bytes)
        .expect_err("a send_test_drip_bytes never fills a frame");
    let read_elapsed_duration = read_started_at.elapsed();

    assert!(
        is_io_timeout(&read_error),
        "the read ended on the deadline, not on the bytes: {read_error:?}"
    );
    assert!(
        read_elapsed_duration < SHORT_TIMEOUT_DURATION + DEADLINE_SLACK_DURATION,
        "the read returned {read_elapsed_duration:?} after it started, inside its {SHORT_TIMEOUT_DURATION:?} deadline, \
         though the peer kept it fed with a byte every {DRIP_INTERVAL_DURATION:?}"
    );
    // Both halves hold the socket, so both go before the send_test_drip_bytes sees it close.
    drop(reader);
    drop(writer);
    let _ = server_thread.join();
}

#[test]
fn a_second_connection_presenting_another_certificate_is_refused_by_the_pinned_fingerprint() {
    let (server_config, certificate_der_bytes) = build_fresh_server_config();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let server_thread = std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return;
        };
        let Ok(server_connection) = ServerConnection::new(Arc::new(server_config)) else {
            return;
        };
        let mut tls_connection = rustls::Connection::Server(server_connection);
        let _ = run_tls_handshake(
            &mut tls_connection,
            &mut socket,
            Instant::now() + LOOPBACK_TIMEOUT_DURATION,
        );
    });

    // The fingerprint of a certificate this server does not hold.
    let pinned_certificate_fingerprint =
        compute_certificate_fingerprint(b"a certificate from another machine");
    let ipc_error = connect_tls_stream(
        &server_address.to_string(),
        Some(&pinned_certificate_fingerprint),
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect_err("a changed certificate is refused");

    let IpcError::CertificateChanged {
        server_address: named_server_address,
        pinned_certificate,
        presented_certificate,
    } = ipc_error
    else {
        panic!("a changed certificate is its own refusal, not a transport failure");
    };
    assert_eq!(named_server_address, server_address.to_string());
    assert_eq!(pinned_certificate, pinned_certificate_fingerprint);
    assert_eq!(
        presented_certificate,
        compute_certificate_fingerprint(&certificate_der_bytes)
    );
    let _ = server_thread.join();
}

/// The opening frame a dialling client sends.
fn build_opening_frame() -> RemoteClientFrame {
    RemoteClientFrame::Hello {
        min_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        max_remote_version: REMOTE_PROTOCOL_VERSION,
        min_protocol_version: 1,
        max_protocol_version: 1,
        connection_token: ConnectionToken::from_secret("the secret the operator handed out"),
    }
}

#[test]
fn a_server_that_drips_its_answer_ends_the_opening_exchange_at_the_deadline() {
    let (server_config, _) = build_fresh_server_config();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let server_thread = std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return;
        };
        let Ok(server_connection) = ServerConnection::new(Arc::new(server_config)) else {
            return;
        };
        let mut tls_connection = rustls::Connection::Server(server_connection);
        if run_tls_handshake(
            &mut tls_connection,
            &mut socket,
            Instant::now() + LOOPBACK_TIMEOUT_DURATION,
        )
        .is_err()
        {
            return;
        }
        // The Hello is never read, and the answer never arrives whole.
        send_test_drip_bytes(&mut socket, build_test_tls_record_header(0x17));
    });

    let exchange_started_at = Instant::now();
    let ipc_error = open_remote_connection(
        &server_address.to_string(),
        None,
        &build_opening_frame(),
        OPENING_TIMEOUT_DURATION,
        None,
    )
    .expect_err("a send_test_drip_bytes never fills the answer");
    let exchange_elapsed_duration = exchange_started_at.elapsed();

    let IpcError::Transport { error_detail } = ipc_error else {
        panic!("an exchange that ran out of time is a transport failure");
    };
    assert!(
        exchange_elapsed_duration < OPENING_TIMEOUT_DURATION + DEADLINE_SLACK_DURATION,
        "the exchange returned {exchange_elapsed_duration:?} after it started, inside its {OPENING_TIMEOUT_DURATION:?} \
         timeout, though the server kept it fed with a byte every {DRIP_INTERVAL_DURATION:?}: {error_detail}"
    );
    let _ = server_thread.join();
}

#[test]
fn a_caller_that_asked_to_wait_reads_however_long_the_server_takes() {
    let (server_config, _) = build_fresh_server_config();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");
    let session_row = build_remote_session_row();
    let served_session_row = session_row.clone();

    let server_thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept the client");
        let server_connection =
            ServerConnection::new(Arc::new(server_config)).expect("a server connection");
        let mut tls_connection = rustls::Connection::Server(server_connection);
        run_tls_handshake(
            &mut tls_connection,
            &mut socket,
            Instant::now() + LOOPBACK_TIMEOUT_DURATION,
        )
        .expect("the loopback handshake finishes");
        let (reader, writer) =
            split_tls_stream(tls_connection, socket).expect("split the loopback stream");
        let (mut frame_reader, mut frame_writer) = frame_halves(Box::new(reader), Box::new(writer));
        let opening_frame: RemoteClientFrame =
            frame_reader.recv().expect("the client's opening frame");
        frame_writer
            .send(&RemoteServerFrame::Welcome {
                remote_protocol_version: REMOTE_PROTOCOL_VERSION,
            })
            .expect("answer the opening frame");
        std::thread::sleep(PAUSE_AFTER_OPENING_RESPONSE_DURATION);
        frame_writer
            .send(&RemoteServerFrame::Sessions {
                session_rows: vec![served_session_row],
            })
            .expect("send the frame after the pause");
        opening_frame
    });

    let (mut frame_reader, _frame_writer, _presented_certificate_fingerprint, opening_response) =
        open_remote_connection(
            &server_address.to_string(),
            None,
            &build_opening_frame(),
            OPENING_TIMEOUT_DURATION,
            None,
        )
        .expect("the opening exchange finishes");
    assert_eq!(
        opening_response,
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION
        }
    );
    assert_eq!(
        frame_reader
            .recv::<RemoteServerFrame>()
            .expect("read the frame the server sent after the pause"),
        RemoteServerFrame::Sessions {
            session_rows: vec![session_row],
        }
    );

    let opening_frame = server_thread.join().expect("the server thread finished");
    assert_eq!(opening_frame, build_opening_frame());
}

#[test]
fn a_caller_that_asked_for_a_bounded_wait_stops_reading_at_it() {
    // What a one-shot command needs. The server admits the connection and then
    // says nothing more; without the bound the read never returns and the
    // command has nothing to print and no reason to stop.
    let (server_config, _) = build_fresh_server_config();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let server_thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept the client");
        let server_connection =
            ServerConnection::new(Arc::new(server_config)).expect("a server connection");
        let mut tls_connection = rustls::Connection::Server(server_connection);
        run_tls_handshake(
            &mut tls_connection,
            &mut socket,
            Instant::now() + LOOPBACK_TIMEOUT_DURATION,
        )
        .expect("the loopback handshake finishes");
        let (reader, writer) =
            split_tls_stream(tls_connection, socket).expect("split the loopback stream");
        let (mut frame_reader, mut frame_writer) = frame_halves(Box::new(reader), Box::new(writer));
        let _: RemoteClientFrame = frame_reader.recv().expect("the client's opening frame");
        frame_writer
            .send(&RemoteServerFrame::Welcome {
                remote_protocol_version: REMOTE_PROTOCOL_VERSION,
            })
            .expect("answer the opening frame");
        // Admitted, and then nothing. Held open so the client is reading a
        // live connection rather than a closed one.
        std::thread::sleep(PAUSE_AFTER_OPENING_RESPONSE_DURATION * 4);
    });

    let bounded_wait_duration = PAUSE_AFTER_OPENING_RESPONSE_DURATION / 2;
    let (mut frame_reader, _frame_writer, _presented_certificate_fingerprint, opening_response) =
        open_remote_connection(
            &server_address.to_string(),
            None,
            &build_opening_frame(),
            OPENING_TIMEOUT_DURATION,
            Some(bounded_wait_duration),
        )
        .expect("the opening exchange finishes");
    assert_eq!(
        opening_response,
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION
        }
    );

    let read_started_at = Instant::now();
    let read_error = frame_reader
        .recv::<RemoteServerFrame>()
        .expect_err("a server that says nothing more is not waited for");
    let read_elapsed_duration = read_started_at.elapsed();

    assert!(
        read_elapsed_duration < PAUSE_AFTER_OPENING_RESPONSE_DURATION * 3,
        "the read ended on the bound it was given, taking {read_elapsed_duration:?}"
    );
    let IpcError::Transport { .. } = read_error else {
        panic!(
            "the read ran out of time, and did not misread a frame or lose the peer: {read_error}"
        );
    };

    let _ = server_thread.join();
}

#[test]
fn a_framed_half_keeps_the_deadline_it_was_dialled_with_and_can_be_told_to_drop_it() {
    // The seam this pins: `open` hands back boxed halves, and the deadline has
    // to survive that box and still be removable through it. A caller that
    // could not remove it would hold a clock over frames that arrive when a
    // person types; a caller whose deadline the box swallowed would wait for
    // good on a server that admits a connection and then says nothing.
    let (server_config, _) = build_fresh_server_config();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");
    let session_row = build_remote_session_row();
    let served_session_row = session_row.clone();

    let server_thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept the client");
        let server_connection =
            ServerConnection::new(Arc::new(server_config)).expect("a server connection");
        let mut tls_connection = rustls::Connection::Server(server_connection);
        run_tls_handshake(
            &mut tls_connection,
            &mut socket,
            Instant::now() + LOOPBACK_TIMEOUT_DURATION,
        )
        .expect("the loopback handshake finishes");
        let (reader, writer) =
            split_tls_stream(tls_connection, socket).expect("split the loopback stream");
        let (mut frame_reader, mut frame_writer) = frame_halves(Box::new(reader), Box::new(writer));
        let _: RemoteClientFrame = frame_reader.recv().expect("the client's opening frame");
        frame_writer
            .send(&RemoteServerFrame::Welcome {
                remote_protocol_version: REMOTE_PROTOCOL_VERSION,
            })
            .expect("answer the opening frame");
        std::thread::sleep(PAUSE_AFTER_OPENING_RESPONSE_DURATION);
        frame_writer
            .send(&RemoteServerFrame::Sessions {
                session_rows: vec![served_session_row],
            })
            .expect("send the frame after the pause");
    });

    let bounded_wait_duration = PAUSE_AFTER_OPENING_RESPONSE_DURATION / 2;
    let (mut frame_reader, mut frame_writer, _presented_certificate_fingerprint, opening_response) =
        open_remote_connection(
            &server_address.to_string(),
            None,
            &build_opening_frame(),
            OPENING_TIMEOUT_DURATION,
            Some(bounded_wait_duration),
        )
        .expect("the opening exchange finishes");
    assert_eq!(
        opening_response,
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION
        }
    );

    // The deadline came through the box: the server is still pausing, so this
    // read gives up rather than waiting it out.
    let read_error = frame_reader
        .recv::<RemoteServerFrame>()
        .expect_err("the dialled deadline holds through the boxed half");
    let IpcError::Transport { .. } = read_error else {
        panic!("a read that ran out of time is a transport failure, not a lost peer: {read_error}");
    };

    // And it can be taken off through the box: the same server, the same
    // pause, and now the frame is waited for.
    frame_reader.set_deadline(None);
    frame_writer.set_deadline(None);
    assert_eq!(
        frame_reader
            .recv::<RemoteServerFrame>()
            .expect("with no deadline the frame after the pause arrives"),
        RemoteServerFrame::Sessions {
            session_rows: vec![session_row],
        }
    );

    let _ = server_thread.join();
}

/// How many bytes the burst test sends in one go. Far past what one socket
/// read can hand the decryption state at once, so a reader that drops the
/// rest of a socket read loses bytes here.
const BURST_BYTE_COUNT: usize = 400 * 1024;

#[test]
fn a_burst_larger_than_one_socket_read_arrives_whole() {
    let (server_config, _certificate_der_bytes) = build_fresh_server_config();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let sent_bytes: Vec<u8> = (0..BURST_BYTE_COUNT)
        .map(|byte_index| (byte_index % 251) as u8)
        .collect();
    let expected_bytes = sent_bytes.clone();
    let server_thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept the client");
        let server_connection =
            ServerConnection::new(Arc::new(server_config)).expect("a server connection");
        let mut tls_connection = rustls::Connection::Server(server_connection);
        run_tls_handshake(
            &mut tls_connection,
            &mut socket,
            Instant::now() + LOOPBACK_TIMEOUT_DURATION,
        )
        .expect("the loopback handshake finishes");
        let (_reader, mut writer) =
            split_tls_stream(tls_connection, socket).expect("split the loopback stream");
        writer
            .write_all(&expected_bytes)
            .expect("the burst is written");
    });

    let (mut reader, _writer, _presented_certificate_fingerprint) =
        connect_tls_stream(&server_address.to_string(), None, LOOPBACK_TIMEOUT_DURATION)
            .expect("the dial opens");
    reader.set_deadline(Some(Instant::now() + LOOPBACK_TIMEOUT_DURATION));
    let mut received_bytes = vec![0u8; BURST_BYTE_COUNT];
    reader
        .read_exact(&mut received_bytes)
        .expect("every byte of the burst arrives");
    assert_eq!(received_bytes, sent_bytes, "the burst arrived changed");

    server_thread.join().expect("the server thread finished");
}

#[test]
fn the_fingerprint_of_three_bytes_is_their_sha256() {
    assert_eq!(
        compute_certificate_fingerprint(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn the_provider_offers_the_hybrid_group_first_and_the_classical_ones_after() {
    let offered_key_exchange_groups: Vec<NamedGroup> = build_crypto_provider()
        .kx_groups
        .iter()
        .map(|group| group.name())
        .collect();

    assert_eq!(
        offered_key_exchange_groups,
        [
            NamedGroup::X25519MLKEM768,
            NamedGroup::X25519,
            NamedGroup::secp256r1,
            NamedGroup::secp384r1,
        ]
    );
}

#[test]
fn an_address_with_no_port_is_a_lookup_failure_and_no_connection_is_made() {
    let ipc_error = connect_tls_stream("127.0.0.1", None, LOOPBACK_TIMEOUT_DURATION)
        .expect_err("an address with no port names nothing to dial");

    let rendered_error = ipc_error.to_string();
    let IpcError::Transport { error_detail } = ipc_error else {
        panic!("a failed lookup is a transport failure: {ipc_error}");
    };
    assert_eq!(
        error_detail,
        "127.0.0.1 could not be looked up: invalid socket address"
    );
    assert_eq!(
        rendered_error,
        "ipc transport error: 127.0.0.1 could not be looked up: invalid socket address"
    );
}

#[test]
fn an_address_whose_port_is_not_a_number_is_a_lookup_failure() {
    let ipc_error = connect_tls_stream("127.0.0.1:seven", None, LOOPBACK_TIMEOUT_DURATION)
        .expect_err("a port that is not a number names nothing to dial");

    let IpcError::Transport { error_detail } = ipc_error else {
        panic!("a failed lookup is a transport failure: {ipc_error}");
    };
    assert_eq!(
        error_detail,
        "127.0.0.1:seven could not be looked up: invalid port value"
    );
}

#[test]
fn a_server_that_hangs_up_during_the_handshake_is_a_handshake_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let server_thread = std::thread::spawn(move || {
        // Accepted, and dropped before a byte is answered.
        let _ = listener.accept();
    });

    let ipc_error =
        connect_tls_stream(&server_address.to_string(), None, LOOPBACK_TIMEOUT_DURATION)
            .expect_err("a server that hangs up never finishes the handshake");

    let IpcError::TlsHandshakeFailed {
        server_address: named_server_address,
        error_detail,
    } = ipc_error
    else {
        panic!("a peer gone mid-handshake is a handshake failure, not a transport failure");
    };
    assert_eq!(named_server_address, server_address.to_string());
    // The words are the operating system's: end of file on one platform, a
    // reset connection on another.
    assert_ne!(error_detail, "the TLS handshake did not finish in time");
    let _ = server_thread.join();
}

/// A connected loopback socket pair: the dialling end and the accepted end.
fn build_loopback_socket_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let loopback_address = listener.local_addr().expect("read the bound address");
    let dialled_socket = TcpStream::connect(loopback_address).expect("connect to loopback");
    let (accepted_socket, _) = listener.accept().expect("accept the dial");
    (dialled_socket, accepted_socket)
}

#[test]
fn no_deadline_leaves_the_socket_timeouts_as_they_are() {
    let (socket, _peer) = build_loopback_socket_pair();
    socket
        .set_read_timeout(Some(Duration::from_secs(7)))
        .expect("set a read timeout");
    socket
        .set_write_timeout(Some(Duration::from_secs(9)))
        .expect("set a write timeout");

    set_socket_timeouts_until(&socket, None).expect("no deadline is not a failure");

    assert_eq!(
        socket.read_timeout().expect("read the timeout"),
        Some(Duration::from_secs(7))
    );
    assert_eq!(
        socket.write_timeout().expect("read the timeout"),
        Some(Duration::from_secs(9))
    );
}

#[test]
fn a_deadline_already_reached_is_timed_out_and_the_socket_timeouts_stay_unset() {
    let (socket, _peer) = build_loopback_socket_pair();

    let timeout_error = set_socket_timeouts_until(&socket, Some(Instant::now()))
        .expect_err("no time left is a failure");

    assert_eq!(timeout_error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(timeout_error.to_string(), "this step ran out of time");
    assert_eq!(socket.read_timeout().expect("read the timeout"), None);
    assert_eq!(socket.write_timeout().expect("read the timeout"), None);
}

#[test]
fn a_deadline_ahead_sets_both_socket_timeouts_to_the_time_left() {
    let (socket, _peer) = build_loopback_socket_pair();

    set_socket_timeouts_until(&socket, Some(Instant::now() + Duration::from_secs(60)))
        .expect("time left is not a failure");

    let read_timeout = socket
        .read_timeout()
        .expect("read the timeout")
        .expect("a read timeout is set");
    let write_timeout = socket
        .write_timeout()
        .expect("read the timeout")
        .expect("a write timeout is set");
    // The time left shrinks between the call and this look at it.
    assert!(
        read_timeout > Duration::from_secs(59) && read_timeout <= Duration::from_secs(60),
        "the read timeout is the time left: {read_timeout:?}"
    );
    assert!(
        write_timeout > Duration::from_secs(59) && write_timeout <= Duration::from_secs(60),
        "the write timeout is the time left: {write_timeout:?}"
    );
}

/// The client configuration [`connect_tls_stream`] builds, with a verifier that takes any
/// certificate.
fn build_test_client_config() -> ClientConfig {
    ClientConfig::builder_with_provider(build_crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports every default protocol version")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(
            PinVerifier::from_expected_certificate_fingerprint(None),
        ))
        .with_no_client_auth()
}

/// A TLS stream over a loopback socket pair with no handshake run: the
/// dialling end split into its halves, and the accepted end.
fn build_split_tls_without_handshake() -> (TlsReader, TlsWriter, TcpStream) {
    let (dialled, accepted) = build_loopback_socket_pair();
    let client_connection = ClientConnection::new(
        Arc::new(build_test_client_config()),
        build_test_server_name(),
    )
    .expect("a client connection");
    let (reader, writer) = split_tls_stream(rustls::Connection::Client(client_connection), dialled)
        .expect("split the stream");
    (reader, writer, accepted)
}

#[test]
fn giving_a_half_a_deadline_stores_it_and_leaves_the_socket_timeouts_alone() {
    let (mut reader, _writer, _peer) = build_split_tls_without_handshake();
    reader
        .socket
        .set_read_timeout(Some(Duration::from_secs(7)))
        .expect("set a read timeout");
    let deadline = Instant::now() + Duration::from_secs(60);

    reader.set_deadline(Some(deadline));

    assert_eq!(reader.deadline, Some(deadline));
    assert_eq!(
        reader.socket.read_timeout().expect("read the timeout"),
        Some(Duration::from_secs(7))
    );
}

#[test]
fn taking_the_deadline_away_clears_both_timeouts_on_that_halfs_handle() {
    let (mut reader, _writer, _peer) = build_split_tls_without_handshake();
    reader.set_deadline(Some(Instant::now() + Duration::from_secs(60)));
    reader
        .socket
        .set_read_timeout(Some(Duration::from_secs(7)))
        .expect("set a read timeout");
    reader
        .socket
        .set_write_timeout(Some(Duration::from_secs(9)))
        .expect("set a write timeout");

    reader.set_deadline(None);

    assert_eq!(reader.deadline, None);
    assert_eq!(
        reader.socket.read_timeout().expect("read the timeout"),
        None
    );
    assert_eq!(
        reader.socket.write_timeout().expect("read the timeout"),
        None
    );
}

#[test]
fn a_reader_prints_its_socket_and_deadline_and_none_of_its_buffer() {
    let (reader, _writer, _peer) = build_split_tls_without_handshake();

    let rendered_debug = format!("{reader:?}");

    assert!(
        rendered_debug.starts_with("TlsReader { socket: TcpStream {"),
        "{rendered_debug}"
    );
    assert!(
        rendered_debug.ends_with(", deadline: None, .. }"),
        "{rendered_debug}"
    );
}

#[test]
fn a_handshake_whose_deadline_has_passed_is_timed_out_before_the_socket_is_touched() {
    let (mut dialled, peer) = build_loopback_socket_pair();
    let client_connection = ClientConnection::new(
        Arc::new(build_test_client_config()),
        build_test_server_name(),
    )
    .expect("a client connection");
    let mut tls_connection = rustls::Connection::Client(client_connection);

    let timeout_error = run_tls_handshake(&mut tls_connection, &mut dialled, Instant::now())
        .expect_err("a deadline already reached ends the handshake");

    assert_eq!(timeout_error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(
        timeout_error.to_string(),
        "the TLS handshake did not finish in time"
    );
    // Nothing was written: the peer's read finds no byte and ends on its own
    // timeout.
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set a read timeout");
    let mut probe_byte = [0u8; 1];
    let peer_read_error = (&peer)
        .read(&mut probe_byte)
        .expect_err("no byte reached the peer");
    assert!(
        is_io_timeout(&peer_read_error),
        "the peer's read ended on its timeout, not on bytes: {peer_read_error:?}"
    );
}

/// Serve `server_config` on a loopback port and run the handshake on the connection
/// that arrives, then hand the finished stream to `after_tls_handshake`. Returns the
/// address to dial and the thread.
fn serve_after_tls_handshake<T: Send + 'static>(
    server_config: ServerConfig,
    after_tls_handshake: impl FnOnce(rustls::Connection, TcpStream) -> T + Send + 'static,
) -> (String, std::thread::JoinHandle<T>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");
    let server_thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept the client");
        let server_connection =
            ServerConnection::new(Arc::new(server_config)).expect("a server connection");
        let mut tls_connection = rustls::Connection::Server(server_connection);
        run_tls_handshake(
            &mut tls_connection,
            &mut socket,
            Instant::now() + LOOPBACK_TIMEOUT_DURATION,
        )
        .expect("the loopback handshake finishes");
        after_tls_handshake(tls_connection, socket)
    });
    (server_address.to_string(), server_thread)
}

#[test]
fn a_peer_that_closes_the_stream_cleanly_reads_as_end_of_stream_every_time() {
    let (server_config, _certificate_der_bytes) = build_fresh_server_config();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, |mut tls_connection, mut socket| {
            tls_connection.send_close_notify();
            while tls_connection.wants_write() {
                tls_connection
                    .write_tls(&mut socket)
                    .expect("the close is written");
            }
        });

    let (mut reader, _writer, _presented_certificate_fingerprint) =
        connect_tls_stream(&server_address, None, LOOPBACK_TIMEOUT_DURATION)
            .expect("the dial opens");
    let mut probe_byte = [0u8; 1];

    assert_eq!(
        reader
            .read(&mut probe_byte)
            .expect("a clean close is end of stream"),
        0
    );
    assert_eq!(
        reader
            .read(&mut probe_byte)
            .expect("and stays end of stream"),
        0
    );
    server_thread.join().expect("the server thread finished");
}

#[test]
fn a_peer_that_drops_the_socket_without_closing_the_stream_is_an_unexpected_eof_every_time() {
    let (server_config, _certificate_der_bytes) = build_fresh_server_config();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, |_tls_connection, socket| drop(socket));

    let (mut reader, _writer, _presented_certificate_fingerprint) =
        connect_tls_stream(&server_address, None, LOOPBACK_TIMEOUT_DURATION)
            .expect("the dial opens");
    let mut probe_byte = [0u8; 1];

    let unexpected_eof_error = reader
        .read(&mut probe_byte)
        .expect_err("a cut stream is not end of stream");
    assert_eq!(unexpected_eof_error.kind(), io::ErrorKind::UnexpectedEof);
    let repeated_unexpected_eof_error = reader.read(&mut probe_byte).expect_err("and stays cut");
    assert_eq!(
        repeated_unexpected_eof_error.kind(),
        io::ErrorKind::UnexpectedEof
    );
    server_thread.join().expect("the server thread finished");
}

#[test]
fn bytes_that_do_not_decrypt_end_the_read_with_invalid_data() {
    let (server_config, _certificate_der_bytes) = build_fresh_server_config();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, |_tls_connection, mut socket| {
            // A TLS record of application data whose 32 bytes were never encrypted.
            let mut tls_record_bytes = vec![0x17, 0x03, 0x03, 0x00, 0x20];
            tls_record_bytes.extend_from_slice(&[0u8; 32]);
            socket
                .write_all(&tls_record_bytes)
                .expect("the TLS record bytes are written");
        });

    let (mut reader, _writer, _presented_certificate_fingerprint) =
        connect_tls_stream(&server_address, None, LOOPBACK_TIMEOUT_DURATION)
            .expect("the dial opens");
    let mut probe_byte = [0u8; 1];

    let io_error = reader
        .read(&mut probe_byte)
        .expect_err("bytes that do not decrypt are not plaintext");
    assert_eq!(io_error.kind(), io::ErrorKind::InvalidData);
    server_thread.join().expect("the server thread finished");
}

/// How many bytes one plaintext write hands to rustls at most: its send
/// buffer limit, 64 KiB.
const MAX_PLAINTEXT_WRITE_BYTE_COUNT: usize = 64 * 1024;

#[test]
fn one_write_takes_at_most_sixty_four_kib_and_write_all_delivers_the_rest() {
    let (server_config, _certificate_der_bytes) = build_fresh_server_config();
    let sent_bytes: Vec<u8> = (0..MAX_PLAINTEXT_WRITE_BYTE_COUNT + 1000)
        .map(|byte_index| (byte_index % 251) as u8)
        .collect();
    let sent_byte_count = sent_bytes.len();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, move |tls_connection, socket| {
            let (mut reader, _writer) =
                split_tls_stream(tls_connection, socket).expect("split the loopback stream");
            let mut received_bytes = vec![0u8; sent_byte_count];
            reader
                .read_exact(&mut received_bytes)
                .expect("every byte arrives");
            received_bytes
        });

    let (_reader, mut writer, _presented_certificate_fingerprint) =
        connect_tls_stream(&server_address, None, LOOPBACK_TIMEOUT_DURATION)
            .expect("the dial opens");
    let written_byte_count = writer
        .write(&sent_bytes)
        .expect("the first write is taken in part");
    assert_eq!(written_byte_count, MAX_PLAINTEXT_WRITE_BYTE_COUNT);
    writer
        .write_all(&sent_bytes[written_byte_count..])
        .expect("the rest is written");

    assert_eq!(
        server_thread.join().expect("the server thread finished"),
        sent_bytes
    );
}

#[test]
fn a_write_after_a_write_that_ran_out_of_time_still_takes_bytes() {
    // The timed-out write leaves 64 KiB of encrypted bytes queued. The next
    // write drains them first, so it has room for its own plaintext.
    let (server_config, _certificate_der_bytes) = build_fresh_server_config();
    // The server reads the exact byte count the client writes, so it finishes
    // without waiting for end of stream and the client holds the connection
    // open until it has.
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, |tls_connection, socket| {
            let (mut reader, _writer) =
                split_tls_stream(tls_connection, socket).expect("split the loopback stream");
            let mut received = vec![0u8; MAX_PLAINTEXT_WRITE_BYTE_COUNT + 5];
            reader
                .read_exact(&mut received)
                .expect("every byte the client wrote arrives");
            received.len()
        });

    let (reader, mut writer, _presented_certificate_fingerprint) =
        connect_tls_stream(&server_address, None, LOOPBACK_TIMEOUT_DURATION)
            .expect("the dial opens");
    writer.set_deadline(Some(Instant::now()));
    let timeout_error = writer
        .write(&[0u8; MAX_PLAINTEXT_WRITE_BYTE_COUNT])
        .expect_err("no time left ends the write");
    assert_eq!(timeout_error.kind(), io::ErrorKind::TimedOut);

    writer.set_deadline(None);
    assert_eq!(
        writer.write(b"after").expect("with time again it is taken"),
        5
    );

    assert_eq!(
        server_thread.join().expect("the server thread finished"),
        MAX_PLAINTEXT_WRITE_BYTE_COUNT + 5
    );
    drop(writer);
    drop(reader);
}

/// How many bytes the blocked-write test sends: far past what the loopback
/// socket buffers of any platform hold, so the write blocks on a peer that
/// does not read.
const UNREAD_BYTE_COUNT: usize = 32 * 1024 * 1024;

#[test]
fn a_write_to_a_peer_that_does_not_read_ends_at_the_writers_deadline() {
    let (server_config, _certificate_der_bytes) = build_fresh_server_config();
    let (given_up_tx, given_up_rx) = std::sync::mpsc::channel::<()>();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, move |_tls_connection, socket| {
            // Reads nothing, and holds the socket open until the write gave up.
            let _ = given_up_rx.recv_timeout(LOOPBACK_TIMEOUT_DURATION * 3);
            drop(socket);
        });

    let (_reader, mut writer, _presented_certificate_fingerprint) =
        connect_tls_stream(&server_address, None, LOOPBACK_TIMEOUT_DURATION)
            .expect("the dial opens");
    writer.set_deadline(Some(Instant::now() + SHORT_TIMEOUT_DURATION));

    let write_started_at = Instant::now();
    let timeout_error = writer
        .write_all(&vec![0u8; UNREAD_BYTE_COUNT])
        .expect_err("a peer that does not read never takes the bytes");
    let write_elapsed_duration = write_started_at.elapsed();
    let _ = given_up_tx.send(());

    assert!(
        is_io_timeout(&timeout_error),
        "the write ended on the deadline, not on the bytes: {timeout_error:?}"
    );
    assert!(
        write_elapsed_duration < SHORT_TIMEOUT_DURATION + DEADLINE_SLACK_DURATION,
        "the write returned {write_elapsed_duration:?} after it started, inside its {SHORT_TIMEOUT_DURATION:?} deadline"
    );
    server_thread.join().expect("the server thread finished");
}

#[test]
fn less_than_a_millisecond_left_counts_as_no_time_left() {
    let (socket, _peer) = build_loopback_socket_pair();

    let timeout_error =
        set_socket_timeouts_until(&socket, Some(Instant::now() + Duration::from_micros(500)))
            .expect_err("less than a millisecond is no time left");

    assert_eq!(timeout_error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(timeout_error.to_string(), "this step ran out of time");
    assert_eq!(socket.read_timeout().expect("read the timeout"), None);
    assert_eq!(socket.write_timeout().expect("read the timeout"), None);
}

#[test]
fn an_empty_buffer_reads_as_zero_bytes_before_the_socket_is_touched() {
    let (mut reader, _writer, _peer) = build_split_tls_without_handshake();
    // A read that reached the socket would wait for bytes that never come
    // and end on this deadline instead of returning `0`.
    reader.set_deadline(Some(Instant::now() + Duration::from_millis(100)));

    assert_eq!(
        reader
            .read(&mut [])
            .expect("an empty buffer is not a failure"),
        0
    );
}

#[test]
fn a_read_with_no_time_left_is_timed_out_before_the_socket_is_touched() {
    let (mut reader, _writer, peer) = build_split_tls_without_handshake();
    reader.set_deadline(Some(Instant::now()));
    let mut probe_byte = [0u8; 1];

    let io_error = reader
        .read(&mut probe_byte)
        .expect_err("no time left ends the read");

    assert_eq!(io_error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(io_error.to_string(), "this step ran out of time");
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set a read timeout");
    let peer_read_error = (&peer)
        .read(&mut probe_byte)
        .expect_err("no byte reached the peer");
    assert!(
        is_io_timeout(&peer_read_error),
        "the peer's read ended on its timeout, not on bytes: {peer_read_error:?}"
    );
}

#[test]
fn a_write_with_no_time_left_is_timed_out_before_the_socket_is_touched() {
    let (_reader, mut writer, peer) = build_split_tls_without_handshake();
    writer.set_deadline(Some(Instant::now()));

    let io_error = writer
        .write(b"never sent")
        .expect_err("no time left ends the write");

    assert_eq!(io_error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(io_error.to_string(), "this step ran out of time");
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set a read timeout");
    let mut probe_byte = [0u8; 1];
    let peer_read_error = (&peer)
        .read(&mut probe_byte)
        .expect_err("no byte reached the peer");
    assert!(
        is_io_timeout(&peer_read_error),
        "the peer's read ended on its timeout, not on bytes: {peer_read_error:?}"
    );
}

#[test]
fn an_empty_write_takes_zero_bytes() {
    let (_reader, mut writer, _peer) = build_split_tls_without_handshake();

    assert_eq!(
        writer.write(b"").expect("an empty write is not a failure"),
        0
    );
}

#[test]
fn the_deadline_trait_reaches_the_halves_own_deadline() {
    let (mut reader, mut writer, _peer) = build_split_tls_without_handshake();
    let deadline = Instant::now() + Duration::from_secs(60);
    writer
        .socket
        .set_write_timeout(Some(Duration::from_secs(9)))
        .expect("set a write timeout");

    Deadlined::set_deadline(&mut reader, Some(deadline));
    Deadlined::set_deadline(&mut writer, None);

    assert_eq!(reader.deadline, Some(deadline));
    assert_eq!(writer.deadline, None);
    assert_eq!(
        writer.socket.write_timeout().expect("read the timeout"),
        None
    );
}

#[test]
fn a_peer_whose_bytes_are_not_a_handshake_ends_it_with_invalid_data() {
    let (mut dialled, mut peer) = build_loopback_socket_pair();
    let client = ClientConnection::new(
        Arc::new(build_test_client_config()),
        build_test_server_name(),
    )
    .expect("a client connection");
    let mut tls_connection = rustls::Connection::Client(client);
    peer.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
        .expect("the peer's bytes are written");

    let io_error = run_tls_handshake(
        &mut tls_connection,
        &mut dialled,
        Instant::now() + LOOPBACK_TIMEOUT_DURATION,
    )
    .expect_err("bytes that are not a handshake end it");

    assert_eq!(io_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        io_error.to_string(),
        "received corrupt message of type InvalidContentType"
    );
}

#[test]
fn a_server_whose_bytes_are_not_a_handshake_is_a_handshake_failure_carrying_its_words() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let server_thread = std::thread::spawn(move || {
        let Ok((mut socket, _)) = listener.accept() else {
            return;
        };
        let _ = socket.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        // Held open until the client has read the answer and hung up.
        let mut remaining_bytes = Vec::new();
        let _ = socket.read_to_end(&mut remaining_bytes);
    });

    let ipc_error =
        connect_tls_stream(&server_address.to_string(), None, LOOPBACK_TIMEOUT_DURATION)
            .expect_err("a server that does not speak TLS never finishes the handshake");

    let IpcError::TlsHandshakeFailed {
        server_address: named_server_address,
        error_detail,
    } = ipc_error
    else {
        panic!("bytes that are not a handshake are a handshake failure: {ipc_error}");
    };
    assert_eq!(named_server_address, server_address.to_string());
    assert_eq!(
        error_detail,
        "received corrupt message of type InvalidContentType"
    );
    let _ = server_thread.join();
}

#[test]
fn the_pinned_fingerprint_is_compared_byte_for_byte() {
    let lowercase_certificate_fingerprint =
        compute_certificate_fingerprint(b"the first certificate");
    let uppercase_certificate_fingerprint = lowercase_certificate_fingerprint.to_uppercase();
    let pin_verifier = PinVerifier::from_expected_certificate_fingerprint(Some(
        &uppercase_certificate_fingerprint,
    ));

    let verification_error = verify_test_certificate(&pin_verifier, b"the first certificate")
        .expect_err("an uppercase pin does not match the lowercase fingerprint");

    assert_eq!(
        verification_error,
        rustls::Error::General(format!(
            "the pinned certificate is {uppercase_certificate_fingerprint}, the server presented {lowercase_certificate_fingerprint}"
        ))
    );
    assert_eq!(
        pin_verifier.get_presented_certificate_fingerprint(),
        Some(lowercase_certificate_fingerprint)
    );
}

#[test]
fn a_verifier_remembers_the_last_certificate_it_was_shown() {
    let pin_verifier = PinVerifier::from_expected_certificate_fingerprint(None);
    verify_test_certificate(&pin_verifier, b"the first certificate")
        .expect("a first connection takes any certificate");
    verify_test_certificate(&pin_verifier, b"the second certificate")
        .expect("and the next one too");

    assert_eq!(
        pin_verifier.get_presented_certificate_fingerprint(),
        Some(compute_certificate_fingerprint(b"the second certificate"))
    );
}
