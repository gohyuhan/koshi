//! Tests for the TLS stream: the certificate fingerprint, what the
//! fingerprint verifier accepts and refuses, the refusal of a server that
//! presents the expected certificate without its private key, the protocol
//! version a dial offers, the key exchange groups the provider offers and the
//! one a handshake settles on, the frames that cross a real loopback
//! stream, the cause a failed dial carries and the words it prints, the
//! deadline a handshake, an opening exchange, a read and a write finish
//! inside, against a peer that answers nothing and against one that sends a
//! byte at a time, how a read ends when the peer closes the stream, cuts it
//! or sends bytes that do not decrypt, how much of one write is taken, both
//! ends writing at once over a slow link, the socket timeouts a deadline and
//! an idle timeout set and take away, and the reads and writes an idle timeout
//! ends and lets finish.
//!
//! The loopback tests bind `127.0.0.1:0`, so the operating system picks a free
//! port and two runs of the suite never meet on one address.

use std::io;
use std::net::TcpListener;

use koshi_core::ids::SessionId;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::sign::{CertifiedKey, SingleCertAndKey};
use rustls::{NamedGroup, ServerConfig, ServerConnection};

use super::*;
use crate::protocol::ConnectionToken;
use crate::remote_wire::{
    open_remote_connection, RemoteClientFrame, RemoteServerFrame, RemoteSessionRow,
    MIN_REMOTE_PROTOCOL_VERSION, REMOTE_PROTOCOL_VERSION,
};
use crate::transport::{build_frame_halves, Deadlined};

/// How long a loopback handshake and the frames after it have to finish: 10
/// seconds.
const LOOPBACK_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

/// The timeout the deadline tests give a dial: 300 ms.
const SHORT_TIMEOUT_DURATION: Duration = Duration::from_millis(300);

/// How far past its timeout a dial in the deadline tests may still return: 3
/// seconds. A dial that returns after that fails its test.
const DEADLINE_SLACK_DURATION: Duration = Duration::from_secs(3);

/// The timeout the opening-exchange tests give a dial: 1 second, longer than
/// a loopback handshake takes and shorter than the byte-by-byte send and the
/// pause that follow it.
const OPENING_TIMEOUT_DURATION: Duration = Duration::from_secs(1);

/// How long a server waits before the frame it sends after the answer: 1.5
/// seconds, past [`OPENING_TIMEOUT_DURATION`].
const PAUSE_AFTER_OPENING_RESPONSE_DURATION: Duration = Duration::from_millis(1500);

/// How long [`send_test_drip_bytes`] waits between the bytes it sends.
const DRIP_INTERVAL_DURATION: Duration = Duration::from_millis(50);

/// How many bytes [`send_test_drip_bytes`] sends after the header: 200. At one
/// byte every [`DRIP_INTERVAL_DURATION`] the send lasts 10 seconds, longer than
/// [`SHORT_TIMEOUT_DURATION`] or [`OPENING_TIMEOUT_DURATION`] plus
/// [`DEADLINE_SLACK_DURATION`].
const DRIP_BYTE_COUNT: usize = 200;

/// A fingerprint no test certificate has: 64 `0` characters. The dials that
/// end before a certificate arrives expect it.
const UNMATCHED_CERTIFICATE_FINGERPRINT: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

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

/// Ask `fingerprint_verifier` about `certificate_der_bytes`.
fn verify_test_certificate(
    fingerprint_verifier: &FingerprintVerifier,
    certificate_der_bytes: &[u8],
) -> Result<ServerCertVerified, rustls::Error> {
    fingerprint_verifier.verify_server_cert(
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
fn the_expected_fingerprint_is_taken_and_every_other_one_is_refused() {
    let expected_certificate_fingerprint =
        compute_certificate_fingerprint(b"the first certificate");
    let fingerprint_verifier = FingerprintVerifier::from_expected_certificate_fingerprint(
        &expected_certificate_fingerprint,
    );
    assert_eq!(
        fingerprint_verifier.get_presented_certificate_fingerprint(),
        None
    );
    verify_test_certificate(&fingerprint_verifier, b"the first certificate")
        .expect("the expected certificate is taken");
    assert_eq!(
        fingerprint_verifier.get_presented_certificate_fingerprint(),
        Some(expected_certificate_fingerprint.clone())
    );

    let fingerprint_verifier = FingerprintVerifier::from_expected_certificate_fingerprint(
        &expected_certificate_fingerprint,
    );
    let verification_error = verify_test_certificate(&fingerprint_verifier, b"another certificate")
        .expect_err("another certificate is refused");
    assert_eq!(
        verification_error,
        rustls::Error::General(format!(
            "the expected certificate is {expected_certificate_fingerprint}, the server presented {}",
            compute_certificate_fingerprint(b"another certificate")
        ))
    );
    assert_eq!(
        fingerprint_verifier.get_presented_certificate_fingerprint(),
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

    let ipc_error = connect_tls_stream(
        &server_address.to_string(),
        UNMATCHED_CERTIFICATE_FINGERPRINT,
        LOOPBACK_TIMEOUT_DURATION,
    )
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

    let ipc_error = connect_tls_stream(
        server_address,
        UNMATCHED_CERTIFICATE_FINGERPRINT,
        Duration::ZERO,
    )
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
    let ipc_error = connect_tls_stream(
        &server_address.to_string(),
        UNMATCHED_CERTIFICATE_FINGERPRINT,
        SHORT_TIMEOUT_DURATION,
    )
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
    let ipc_error = connect_tls_stream(
        &server_address.to_string(),
        UNMATCHED_CERTIFICATE_FINGERPRINT,
        SHORT_TIMEOUT_DURATION,
    )
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

/// A server configuration serving a fresh self-signed certificate with
/// `crypto_provider`, and the fingerprint of that certificate.
fn build_server_config_with_provider(
    crypto_provider: Arc<CryptoProvider>,
) -> (ServerConfig, String) {
    let generated_certificate = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .expect("generate a self-signed certificate");
    let certificate_der_bytes = generated_certificate.cert.der().to_vec();
    let certificate_fingerprint = compute_certificate_fingerprint(&certificate_der_bytes);
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        generated_certificate.signing_key.serialize_der(),
    ));
    let server_config = ServerConfig::builder_with_provider(crypto_provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("aws-lc-rs supports TLS 1.3")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate_der_bytes)],
            private_key,
        )
        .expect("a certificate and its key make a server configuration");
    (server_config, certificate_fingerprint)
}

/// A TLS configuration serving a fresh self-signed certificate, the way this
/// machine's own certificate is served, and the fingerprint of that
/// certificate.
fn build_fresh_server_config() -> (ServerConfig, String) {
    build_server_config_with_provider(build_crypto_provider())
}

/// A server configuration whose key exchange list holds `X25519` alone, and
/// the fingerprint of the fresh self-signed certificate it serves.
fn build_classical_only_server_config() -> (ServerConfig, String) {
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

/// Serve `server_config` on a loopback port, dial it expecting
/// `certificate_fingerprint`, and report the key exchange group the handshake
/// settled on.
fn negotiate_key_exchange_group(
    server_config: ServerConfig,
    certificate_fingerprint: &str,
) -> Option<NamedGroup> {
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

    let (_reader, _writer) = connect_tls_stream(
        &server_address.to_string(),
        certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect("the dial opens");
    server_thread.join().expect("the server thread finishes")
}

/// Two koshi peers settle on `X25519MLKEM768`: the X25519 elliptic curve
/// combined with ML-KEM-768, the post-quantum key encapsulation mechanism.
/// koshi offers that group ahead of every classical one.
#[test]
fn a_loopback_handshake_settles_on_the_hybrid_post_quantum_key_exchange() {
    let (server_config, certificate_fingerprint) = build_fresh_server_config();

    assert_eq!(
        negotiate_key_exchange_group(server_config, &certificate_fingerprint),
        Some(NamedGroup::X25519MLKEM768)
    );
}

/// A server whose key exchange list holds `X25519` alone accepts the dial,
/// and the handshake settles on `X25519` rather than failing.
#[test]
fn a_server_that_offers_only_classical_key_exchange_still_accepts_a_dial() {
    let (server_config, certificate_fingerprint) = build_classical_only_server_config();

    assert_eq!(
        negotiate_key_exchange_group(server_config, &certificate_fingerprint),
        Some(NamedGroup::X25519)
    );
}

#[test]
fn frames_cross_a_loopback_stream_both_ways_once_the_expected_certificate_is_shown() {
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
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
        let (mut frame_reader, mut frame_writer) =
            build_frame_halves(Box::new(reader), Box::new(writer));

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

    let (reader, writer) = connect_tls_stream(
        &server_address.to_string(),
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect("the dial opens");
    let (mut frame_reader, mut frame_writer) =
        build_frame_halves(Box::new(reader), Box::new(writer));

    let hello_frame = RemoteClientFrame::Hello {
        minimum_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        maximum_remote_version: REMOTE_PROTOCOL_VERSION,
        minimum_protocol_version: 1,
        maximum_protocol_version: 1,
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
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

    let (mut reader, writer) = connect_tls_stream(
        &server_address.to_string(),
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect("the dial opens");
    reader.set_deadline(Some(Instant::now() + SHORT_TIMEOUT_DURATION));

    let read_started_at = Instant::now();
    let mut frame_length_bytes = [0u8; 4];
    let read_error = reader
        .read_exact(&mut frame_length_bytes)
        .expect_err("bytes sent one at a time never fill a frame");
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
    // Both halves hold the socket, so both go before the byte-by-byte sender sees it close.
    drop(reader);
    drop(writer);
    let _ = server_thread.join();
}

#[test]
fn a_server_presenting_another_certificate_than_the_expected_one_is_refused() {
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
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
    let expected_certificate_fingerprint =
        compute_certificate_fingerprint(b"a certificate from another machine");
    let ipc_error = connect_tls_stream(
        &server_address.to_string(),
        &expected_certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect_err("another certificate is refused");

    let IpcError::CertificateMismatch {
        server_address: named_server_address,
        expected_certificate_fingerprint: named_expected_certificate_fingerprint,
        presented_certificate_fingerprint,
    } = ipc_error
    else {
        panic!("another certificate is its own refusal, not a transport failure");
    };
    assert_eq!(named_server_address, server_address.to_string());
    assert_eq!(
        named_expected_certificate_fingerprint,
        expected_certificate_fingerprint
    );
    assert_eq!(presented_certificate_fingerprint, certificate_fingerprint);
    let _ = server_thread.join();
}

#[test]
fn a_server_presenting_the_expected_certificate_without_its_private_key_is_refused() {
    // The server presents one certificate and signs the handshake with the
    // private key of another. The certificate check takes the presented
    // certificate, and the handshake signature check refuses the signature.
    let presented_certificate = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .expect("generate the presented certificate");
    let signing_certificate = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .expect("generate the certificate whose private key signs");
    let presented_certificate_der_bytes = presented_certificate.cert.der().to_vec();
    let presented_certificate_fingerprint =
        compute_certificate_fingerprint(&presented_certificate_der_bytes);
    let crypto_provider = build_crypto_provider();
    let unrelated_signing_key = crypto_provider
        .key_provider
        .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            signing_certificate.signing_key.serialize_der(),
        )))
        .expect("the provider loads the private key");
    let server_config = ServerConfig::builder_with_provider(crypto_provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("aws-lc-rs supports TLS 1.3")
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(SingleCertAndKey::from(CertifiedKey::new(
            vec![CertificateDer::from(presented_certificate_der_bytes)],
            unrelated_signing_key,
        ))));
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

    let ipc_error = connect_tls_stream(
        &server_address.to_string(),
        &presented_certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect_err("a handshake signature the certificate's key did not make is refused");

    let IpcError::TlsHandshakeFailed {
        server_address: named_server_address,
        error_detail,
    } = ipc_error
    else {
        panic!("the presented certificate is the expected one, so the handshake refuses it");
    };
    assert_eq!(named_server_address, server_address.to_string());
    assert_eq!(error_detail, "invalid peer certificate: BadSignature");
    let _ = server_thread.join();
}

/// The type number of the `supported_versions` extension, RFC 8446 section
/// 4.2: `43`.
const SUPPORTED_VERSIONS_EXTENSION_TYPE: u16 = 43;

/// The protocol versions the `supported_versions` extension of one
/// ClientHello names, in the order it names them, or none when the hello
/// carries no such extension.
///
/// `client_hello_bytes` is the ClientHello after its 4-byte handshake header:
/// the 2-byte legacy version, the 32-byte random, the session id, the cipher
/// suites, the compression methods, and the extensions. Example — the
/// extension bytes `00 2b 00 05 04 03 04 03 03` give `[0x0304, 0x0303]`.
fn list_offered_protocol_versions(client_hello_bytes: &[u8]) -> Vec<u16> {
    let read_big_endian_u16 = |byte_index: usize| {
        u16::from_be_bytes([
            client_hello_bytes[byte_index],
            client_hello_bytes[byte_index + 1],
        ])
    };
    let session_id_index = 2 + 32;
    let cipher_suites_index =
        session_id_index + 1 + usize::from(client_hello_bytes[session_id_index]);
    let compression_methods_index =
        cipher_suites_index + 2 + usize::from(read_big_endian_u16(cipher_suites_index));
    let mut extension_index = compression_methods_index
        + 1
        + usize::from(client_hello_bytes[compression_methods_index])
        + 2;
    while extension_index + 4 <= client_hello_bytes.len() {
        let extension_type = read_big_endian_u16(extension_index);
        let extension_byte_count = usize::from(read_big_endian_u16(extension_index + 2));
        let extension_body_index = extension_index + 4;
        if extension_type == SUPPORTED_VERSIONS_EXTENSION_TYPE {
            let version_list_byte_count = usize::from(client_hello_bytes[extension_body_index]);
            return (0..version_list_byte_count / 2)
                .map(|version_index| {
                    read_big_endian_u16(extension_body_index + 1 + version_index * 2)
                })
                .collect();
        }
        extension_index = extension_body_index + extension_byte_count;
    }
    Vec::new()
}

#[test]
fn a_dial_offers_tls_1_3_alone() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    // Reads the first TLS record the client sends, then hangs up.
    let capture_thread = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept the client");
        let mut record_header = [0u8; 5];
        socket
            .read_exact(&mut record_header)
            .expect("the record header arrives");
        let record_byte_count =
            usize::from(u16::from_be_bytes([record_header[3], record_header[4]]));
        let mut record_body = vec![0u8; record_byte_count];
        socket
            .read_exact(&mut record_body)
            .expect("the record arrives whole");
        (record_header[0], record_body)
    });

    let ipc_error = connect_tls_stream(
        &server_address.to_string(),
        UNMATCHED_CERTIFICATE_FINGERPRINT,
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect_err("a server that hangs up after the first record never finishes the handshake");
    let (record_type, record_body) = capture_thread.join().expect("the capture thread finished");

    let IpcError::TlsHandshakeFailed {
        server_address: named_server_address,
        ..
    } = ipc_error
    else {
        panic!("a peer gone mid-handshake is a handshake failure: {ipc_error}");
    };
    assert_eq!(named_server_address, server_address.to_string());
    // Record type 22 is a handshake record, and handshake type 1 a ClientHello.
    assert_eq!(record_type, 22);
    assert_eq!(record_body[0], 1);
    assert_eq!(
        list_offered_protocol_versions(&record_body[4..]),
        vec![u16::from(rustls::ProtocolVersion::TLSv1_3)]
    );
}

/// The opening frame a dialling client sends.
fn build_opening_frame() -> RemoteClientFrame {
    RemoteClientFrame::Hello {
        minimum_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        maximum_remote_version: REMOTE_PROTOCOL_VERSION,
        minimum_protocol_version: 1,
        maximum_protocol_version: 1,
        connection_token: ConnectionToken::from_secret("the secret the operator handed out"),
    }
}

#[test]
fn a_server_that_drips_its_answer_ends_the_opening_exchange_at_the_deadline() {
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
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
        &certificate_fingerprint,
        &build_opening_frame(),
        OPENING_TIMEOUT_DURATION,
        None,
    )
    .expect_err("bytes sent one at a time never fill the answer");
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
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
        let (mut frame_reader, mut frame_writer) =
            build_frame_halves(Box::new(reader), Box::new(writer));
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

    let (mut frame_reader, _frame_writer, opening_response) = open_remote_connection(
        &server_address.to_string(),
        &certificate_fingerprint,
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
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
        let (mut frame_reader, mut frame_writer) =
            build_frame_halves(Box::new(reader), Box::new(writer));
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
    let (mut frame_reader, _frame_writer, opening_response) = open_remote_connection(
        &server_address.to_string(),
        &certificate_fingerprint,
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
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
        let (mut frame_reader, mut frame_writer) =
            build_frame_halves(Box::new(reader), Box::new(writer));
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
    let (mut frame_reader, mut frame_writer, opening_response) = open_remote_connection(
        &server_address.to_string(),
        &certificate_fingerprint,
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let server_address = listener.local_addr().expect("read the bound address");

    let sent_bytes = build_counting_bytes(BURST_BYTE_COUNT);
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

    let (mut reader, _writer) = connect_tls_stream(
        &server_address.to_string(),
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
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
    let ipc_error = connect_tls_stream(
        "127.0.0.1",
        UNMATCHED_CERTIFICATE_FINGERPRINT,
        LOOPBACK_TIMEOUT_DURATION,
    )
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
    let ipc_error = connect_tls_stream(
        "127.0.0.1:seven",
        UNMATCHED_CERTIFICATE_FINGERPRINT,
        LOOPBACK_TIMEOUT_DURATION,
    )
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

    let ipc_error = connect_tls_stream(
        &server_address.to_string(),
        UNMATCHED_CERTIFICATE_FINGERPRINT,
        LOOPBACK_TIMEOUT_DURATION,
    )
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

    set_socket_timeout_until(&socket, None, TcpStream::set_read_timeout)
        .expect("no deadline is not a failure");
    set_socket_timeout_until(&socket, None, TcpStream::set_write_timeout)
        .expect("no deadline is not a failure");

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

    let timeout_error =
        set_socket_timeout_until(&socket, Some(Instant::now()), TcpStream::set_read_timeout)
            .expect_err("no time left is a failure");

    assert_eq!(timeout_error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(timeout_error.to_string(), "this step ran out of time");
    assert_eq!(socket.read_timeout().expect("read the timeout"), None);
    assert_eq!(socket.write_timeout().expect("read the timeout"), None);
}

#[test]
fn a_deadline_ahead_sets_only_the_named_socket_timeout_to_the_time_left() {
    let (socket, _peer) = build_loopback_socket_pair();

    set_socket_timeout_until(
        &socket,
        Some(Instant::now() + Duration::from_secs(60)),
        TcpStream::set_read_timeout,
    )
    .expect("time left is not a failure");

    let read_timeout = socket
        .read_timeout()
        .expect("read the timeout")
        .expect("a read timeout is set");
    // The time left shrinks between the call and this look at it.
    assert!(
        read_timeout > Duration::from_secs(59) && read_timeout <= Duration::from_secs(60),
        "the read timeout is the time left: {read_timeout:?}"
    );
    assert_eq!(socket.write_timeout().expect("read the timeout"), None);

    set_socket_timeout_until(
        &socket,
        Some(Instant::now() + Duration::from_secs(30)),
        TcpStream::set_write_timeout,
    )
    .expect("time left is not a failure");

    let write_timeout = socket
        .write_timeout()
        .expect("read the timeout")
        .expect("a write timeout is set");
    assert!(
        write_timeout > Duration::from_secs(29) && write_timeout <= Duration::from_secs(30),
        "the write timeout is the time left: {write_timeout:?}"
    );
    assert_eq!(
        socket.read_timeout().expect("read the timeout"),
        Some(read_timeout)
    );
}

/// The client configuration [`connect_tls_stream`] builds, with a verifier that expects
/// [`UNMATCHED_CERTIFICATE_FINGERPRINT`].
fn build_test_client_config() -> ClientConfig {
    ClientConfig::builder_with_provider(build_crypto_provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("aws-lc-rs supports TLS 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(
            FingerprintVerifier::from_expected_certificate_fingerprint(
                UNMATCHED_CERTIFICATE_FINGERPRINT,
            ),
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
fn taking_a_halfs_deadline_away_clears_only_its_own_timeout_on_its_handle() {
    let (mut reader, mut writer, _peer) = build_split_tls_without_handshake();
    for half_socket in [&reader.socket, &writer.socket] {
        half_socket
            .set_read_timeout(Some(Duration::from_secs(7)))
            .expect("set a read timeout");
        half_socket
            .set_write_timeout(Some(Duration::from_secs(9)))
            .expect("set a write timeout");
    }
    reader.set_deadline(Some(Instant::now() + Duration::from_secs(60)));

    reader.set_deadline(None);

    assert_eq!(reader.deadline, None);
    assert_eq!(
        reader.socket.read_timeout().expect("read the timeout"),
        None
    );
    assert_eq!(
        reader.socket.write_timeout().expect("read the timeout"),
        Some(Duration::from_secs(9))
    );

    writer
        .socket
        .set_read_timeout(Some(Duration::from_secs(7)))
        .expect("set a read timeout");
    writer.set_deadline(Some(Instant::now() + Duration::from_secs(60)));

    writer.set_deadline(None);

    assert_eq!(writer.deadline, None);
    assert_eq!(
        writer.socket.write_timeout().expect("read the timeout"),
        None
    );
    assert_eq!(
        writer.socket.read_timeout().expect("read the timeout"),
        Some(Duration::from_secs(7))
    );
}

#[test]
fn giving_a_half_an_idle_timeout_stores_it_and_leaves_the_socket_timeouts_alone() {
    let (mut reader, mut writer, _peer) = build_split_tls_without_handshake();
    reader
        .socket
        .set_read_timeout(Some(Duration::from_secs(7)))
        .expect("set a read timeout");
    writer
        .socket
        .set_write_timeout(Some(Duration::from_secs(9)))
        .expect("set a write timeout");

    reader.set_idle_timeout(Some(Duration::from_secs(30)));
    writer.set_idle_timeout(Some(Duration::from_secs(20)));

    assert_eq!(reader.idle_timeout, Some(Duration::from_secs(30)));
    assert_eq!(writer.idle_timeout, Some(Duration::from_secs(20)));
    assert_eq!(
        reader.socket.read_timeout().expect("read the timeout"),
        Some(Duration::from_secs(7))
    );
    assert_eq!(
        writer.socket.write_timeout().expect("read the timeout"),
        Some(Duration::from_secs(9))
    );
}

#[test]
fn taking_a_halfs_idle_timeout_away_clears_only_its_own_timeout_on_its_handle() {
    let (mut reader, mut writer, _peer) = build_split_tls_without_handshake();
    for half_socket in [&reader.socket, &writer.socket] {
        half_socket
            .set_read_timeout(Some(Duration::from_secs(7)))
            .expect("set a read timeout");
        half_socket
            .set_write_timeout(Some(Duration::from_secs(9)))
            .expect("set a write timeout");
    }
    reader.set_idle_timeout(Some(Duration::from_secs(30)));

    reader.set_idle_timeout(None);

    assert_eq!(reader.idle_timeout, None);
    assert_eq!(
        reader.socket.read_timeout().expect("read the timeout"),
        None
    );
    assert_eq!(
        reader.socket.write_timeout().expect("read the timeout"),
        Some(Duration::from_secs(9))
    );

    writer
        .socket
        .set_read_timeout(Some(Duration::from_secs(7)))
        .expect("set a read timeout");
    writer.set_idle_timeout(Some(Duration::from_secs(30)));

    writer.set_idle_timeout(None);

    assert_eq!(writer.idle_timeout, None);
    assert_eq!(
        writer.socket.write_timeout().expect("read the timeout"),
        None
    );
    assert_eq!(
        writer.socket.read_timeout().expect("read the timeout"),
        Some(Duration::from_secs(7))
    );
}

#[test]
fn a_zero_idle_timeout_ends_a_read_and_a_write_before_the_socket_is_touched() {
    let (mut reader, mut writer, peer) = build_split_tls_without_handshake();
    reader.set_idle_timeout(Some(Duration::ZERO));
    writer.set_idle_timeout(Some(Duration::ZERO));
    let mut probe_byte = [0u8; 1];

    let read_error = reader
        .read(&mut probe_byte)
        .expect_err("a zero idle timeout leaves no time to read");
    let write_error = writer
        .write(b"never sent")
        .expect_err("a zero idle timeout leaves no time to write");

    assert_eq!(read_error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(read_error.to_string(), "this step ran out of time");
    assert_eq!(write_error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(write_error.to_string(), "this step ran out of time");
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
fn a_read_under_the_readers_deadline_leaves_the_write_timeout_unset() {
    let (mut reader, writer, _peer) = build_split_tls_without_handshake();
    reader.set_deadline(Some(Instant::now() + Duration::from_millis(100)));
    let mut probe_byte = [0u8; 1];

    let read_error = reader
        .read(&mut probe_byte)
        .expect_err("the peer sends nothing");

    assert!(
        is_io_timeout(&read_error),
        "the read ended at the reader's deadline: {read_error:?}"
    );
    assert_eq!(
        writer.socket.write_timeout().expect("read the timeout"),
        None
    );
}

#[test]
fn a_reader_prints_its_socket_deadline_and_idle_timeout_and_none_of_its_buffer() {
    let (reader, _writer, _peer) = build_split_tls_without_handshake();

    let rendered_debug = format!("{reader:?}");

    assert!(
        rendered_debug.starts_with("TlsReader { socket: TcpStream {"),
        "{rendered_debug}"
    );
    assert!(
        rendered_debug.ends_with(", deadline: None, idle_timeout: None, .. }"),
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, |mut tls_connection, mut socket| {
            tls_connection.send_close_notify();
            while tls_connection.wants_write() {
                tls_connection
                    .write_tls(&mut socket)
                    .expect("the close is written");
            }
        });

    let (mut reader, _writer) = connect_tls_stream(
        &server_address,
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, |_tls_connection, socket| drop(socket));

    let (mut reader, _writer) = connect_tls_stream(
        &server_address,
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, |_tls_connection, mut socket| {
            // A TLS record of application data whose 32 bytes were never encrypted.
            let mut tls_record_bytes = vec![0x17, 0x03, 0x03, 0x00, 0x20];
            tls_record_bytes.extend_from_slice(&[0u8; 32]);
            socket
                .write_all(&tls_record_bytes)
                .expect("the TLS record bytes are written");
        });

    let (mut reader, _writer) = connect_tls_stream(
        &server_address,
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
    let sent_bytes = build_counting_bytes(MAX_PLAINTEXT_WRITE_BYTE_COUNT + 1000);
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

    let (_reader, mut writer) = connect_tls_stream(
        &server_address,
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
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

    let (reader, mut writer) = connect_tls_stream(
        &server_address,
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
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
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
    let (given_up_sender, given_up_receiver) = std::sync::mpsc::channel::<()>();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, move |_tls_connection, socket| {
            // Reads nothing, and holds the socket open until the write gave up.
            let _ = given_up_receiver.recv_timeout(LOOPBACK_TIMEOUT_DURATION * 3);
            drop(socket);
        });

    let (_reader, mut writer) = connect_tls_stream(
        &server_address,
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect("the dial opens");
    writer.set_deadline(Some(Instant::now() + SHORT_TIMEOUT_DURATION));

    let write_started_at = Instant::now();
    let timeout_error = writer
        .write_all(&vec![0u8; UNREAD_BYTE_COUNT])
        .expect_err("a peer that does not read never takes the bytes");
    let write_elapsed_duration = write_started_at.elapsed();
    let _ = given_up_sender.send(());

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
fn a_read_whose_bytes_arrive_within_the_idle_timeout_of_each_other_is_read_whole() {
    // The server sends the 16 KiB record in twenty pieces 100 ms apart once
    // the read starts. The last piece leaves 1.9 seconds after the read
    // starts, later than the 1-second idle timeout, and no piece leaves more
    // than 100 ms after the one before it.
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
    let sent_plaintext = build_counting_bytes(16 * 1024);
    let server_plaintext = sent_plaintext.clone();
    let (read_started_sender, read_started_receiver) = std::sync::mpsc::channel::<()>();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, move |mut tls_connection, mut socket| {
            tls_connection
                .writer()
                .write_all(&server_plaintext)
                .expect("the plaintext is taken");
            let mut encrypted_bytes = Vec::new();
            while tls_connection.wants_write() {
                tls_connection
                    .write_tls(&mut encrypted_bytes)
                    .expect("the records are taken");
            }
            read_started_receiver
                .recv_timeout(LOOPBACK_TIMEOUT_DURATION)
                .expect("the read starts");
            let piece_byte_count = encrypted_bytes.len().div_ceil(20);
            for (piece_index, encrypted_piece) in
                encrypted_bytes.chunks(piece_byte_count).enumerate()
            {
                if piece_index > 0 {
                    std::thread::sleep(Duration::from_millis(100));
                }
                socket
                    .write_all(encrypted_piece)
                    .expect("the piece is written");
            }
            socket
        });

    let (mut reader, _writer) = connect_tls_stream(
        &server_address,
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect("the dial opens");
    reader.set_deadline(None);
    reader.set_idle_timeout(Some(Duration::from_secs(1)));
    let read_started_at = Instant::now();
    read_started_sender
        .send(())
        .expect("the server waits for the read");
    let mut received_plaintext = vec![0u8; sent_plaintext.len()];

    reader
        .read_exact(&mut received_plaintext)
        .expect("every byte arrives");

    assert_eq!(received_plaintext, sent_plaintext);
    assert!(
        read_started_at.elapsed() > Duration::from_secs(1),
        "the plaintext took longer than one idle timeout to arrive"
    );
    drop(server_thread.join().expect("the server thread finished"));
}

#[test]
fn each_socket_write_waits_at_most_the_earlier_of_the_deadline_and_the_idle_timeout() {
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
    let (written_sender, written_receiver) = std::sync::mpsc::channel::<()>();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, move |_tls_connection, socket| {
            // Reads nothing, and holds the socket open until both writes are
            // taken.
            let _ = written_receiver.recv_timeout(LOOPBACK_TIMEOUT_DURATION);
            drop(socket);
        });
    let (_reader, mut writer) = connect_tls_stream(
        &server_address,
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect("the dial opens");
    writer.set_deadline(None);
    writer.set_idle_timeout(Some(Duration::from_secs(30)));

    writer
        .write_all(b"first")
        .expect("the first write is taken");

    let idle_write_timeout = writer
        .socket
        .write_timeout()
        .expect("read the timeout")
        .expect("a write timeout is set");
    assert!(
        idle_write_timeout > Duration::from_secs(29)
            && idle_write_timeout <= Duration::from_secs(30),
        "the socket write waits at most the idle timeout: {idle_write_timeout:?}"
    );

    writer.set_deadline(Some(Instant::now() + Duration::from_secs(10)));
    writer
        .write_all(b"second")
        .expect("the second write is taken");

    let deadline_write_timeout = writer
        .socket
        .write_timeout()
        .expect("read the timeout")
        .expect("a write timeout is set");
    assert!(
        deadline_write_timeout > Duration::from_secs(9)
            && deadline_write_timeout <= Duration::from_secs(10),
        "the socket write waits at most the time left until the deadline: {deadline_write_timeout:?}"
    );
    let _ = written_sender.send(());
    server_thread.join().expect("the server thread finished");
}

/// How many bytes each end writes in the two-way test: far past what the
/// socket buffers between the two ends hold, so each writer blocks on a full
/// socket while the other end writes too.
const TWO_WAY_BYTE_COUNT: usize = 8 * 1024 * 1024;

/// The most bytes the slow link carries in one step.
const SLOW_LINK_CHUNK_BYTE_COUNT: usize = 4096;

/// How long the slow link waits after each step.
const SLOW_LINK_PAUSE_DURATION: Duration = Duration::from_millis(1);

/// `byte_count` bytes that count from `0` to `250` and then start again at
/// `0`: the byte at index `n` is `n % 251`.
fn build_counting_bytes(byte_count: usize) -> Vec<u8> {
    (0..byte_count)
        .map(|byte_index| u8::try_from(byte_index % 251).expect("a value below 251 fits in u8"))
        .collect()
}

/// Copy the bytes `source_socket` receives onto `destination_socket`, at most
/// [`SLOW_LINK_CHUNK_BYTE_COUNT`] bytes per step with a
/// [`SLOW_LINK_PAUSE_DURATION`] pause after each step. Stops at the end of
/// `source_socket`'s stream or at the first failure, then shuts the writing
/// direction of `destination_socket`.
fn carry_bytes_slowly(mut source_socket: TcpStream, mut destination_socket: TcpStream) {
    let mut carried_chunk = [0u8; SLOW_LINK_CHUNK_BYTE_COUNT];
    loop {
        let carried_byte_count = match source_socket.read(&mut carried_chunk) {
            Ok(0) | Err(_) => break,
            Ok(carried_byte_count) => carried_byte_count,
        };
        if destination_socket
            .write_all(&carried_chunk[..carried_byte_count])
            .is_err()
        {
            break;
        }
        std::thread::sleep(SLOW_LINK_PAUSE_DURATION);
    }
    let _ = destination_socket.shutdown(std::net::Shutdown::Write);
}

/// Listen on a loopback port and join the one connection that arrives to
/// `server_address` through [`carry_bytes_slowly`], one thread per direction.
/// Returns the address to dial.
fn start_slow_link_to(server_address: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let slow_link_address = listener.local_addr().expect("read the bound address");
    let server_address = server_address.to_string();
    std::thread::spawn(move || {
        let (dialled_socket, _) = listener.accept().expect("accept the client");
        let server_socket = TcpStream::connect(&server_address).expect("connect to the server");
        let dialled_socket_copy = dialled_socket.try_clone().expect("clone the client socket");
        let server_socket_copy = server_socket.try_clone().expect("clone the server socket");
        std::thread::spawn(move || carry_bytes_slowly(dialled_socket_copy, server_socket_copy));
        carry_bytes_slowly(server_socket, dialled_socket);
    });
    slow_link_address.to_string()
}

#[test]
fn both_ends_writing_more_than_a_slow_link_holds_at_once_deliver_every_byte_both_ways() {
    let (server_config, certificate_fingerprint) = build_fresh_server_config();
    let (server_address, server_thread) =
        serve_after_tls_handshake(server_config, |tls_connection, socket| {
            split_tls_stream(tls_connection, socket).expect("split the loopback stream")
        });
    let slow_link_address = start_slow_link_to(&server_address);
    let (mut client_reader, mut client_writer) = connect_tls_stream(
        &slow_link_address,
        &certificate_fingerprint,
        LOOPBACK_TIMEOUT_DURATION,
    )
    .expect("the dial opens");
    let (mut server_reader, mut server_writer) =
        server_thread.join().expect("the server thread finished");
    client_reader.set_deadline(None);
    client_writer.set_deadline(None);
    server_reader.set_deadline(None);
    server_writer.set_deadline(None);
    let (finished_half_sender, finished_half_receiver) =
        std::sync::mpsc::channel::<(&'static str, bool)>();

    for (half_name, mut writer) in [
        ("client writer", client_writer),
        ("server writer", server_writer),
    ] {
        let finished_half_sender = finished_half_sender.clone();
        std::thread::spawn(move || {
            let is_written = writer
                .write_all(&build_counting_bytes(TWO_WAY_BYTE_COUNT))
                .is_ok();
            let _ = finished_half_sender.send((half_name, is_written));
            writer
        });
    }
    for (half_name, mut reader) in [
        ("client reader", client_reader),
        ("server reader", server_reader),
    ] {
        let finished_half_sender = finished_half_sender.clone();
        std::thread::spawn(move || {
            let mut received_bytes = vec![0u8; TWO_WAY_BYTE_COUNT];
            let is_read = reader.read_exact(&mut received_bytes).is_ok()
                && received_bytes == build_counting_bytes(TWO_WAY_BYTE_COUNT);
            let _ = finished_half_sender.send((half_name, is_read));
            reader
        });
    }

    let mut finished_halves: Vec<(&'static str, bool)> = (0..4)
        .map(|_| {
            finished_half_receiver
                .recv_timeout(LOOPBACK_TIMEOUT_DURATION * 3)
                .expect("every half finishes: no end waits on the other forever")
        })
        .collect();
    finished_halves.sort_unstable();
    assert_eq!(
        finished_halves,
        vec![
            ("client reader", true),
            ("client writer", true),
            ("server reader", true),
            ("server writer", true),
        ]
    );
}

#[test]
fn less_than_a_millisecond_left_counts_as_no_time_left() {
    let (socket, _peer) = build_loopback_socket_pair();

    let timeout_error = set_socket_timeout_until(
        &socket,
        Some(Instant::now() + Duration::from_micros(500)),
        TcpStream::set_write_timeout,
    )
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
fn the_deadline_trait_reaches_the_halves_own_deadline_and_idle_timeout() {
    let (mut reader, mut writer, _peer) = build_split_tls_without_handshake();
    let deadline = Instant::now() + Duration::from_secs(60);
    let idle_timeout = Duration::from_secs(30);
    writer
        .socket
        .set_write_timeout(Some(Duration::from_secs(9)))
        .expect("set a write timeout");
    reader
        .socket
        .set_read_timeout(Some(Duration::from_secs(9)))
        .expect("set a read timeout");

    Deadlined::set_deadline(&mut reader, Some(deadline));
    Deadlined::set_deadline(&mut writer, None);
    Deadlined::set_idle_timeout(&mut reader, None);
    Deadlined::set_idle_timeout(&mut writer, Some(idle_timeout));

    assert_eq!(reader.deadline, Some(deadline));
    assert_eq!(writer.deadline, None);
    assert_eq!(reader.idle_timeout, None);
    assert_eq!(writer.idle_timeout, Some(idle_timeout));
    assert_eq!(
        reader.socket.read_timeout().expect("read the timeout"),
        None
    );
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

    let ipc_error = connect_tls_stream(
        &server_address.to_string(),
        UNMATCHED_CERTIFICATE_FINGERPRINT,
        LOOPBACK_TIMEOUT_DURATION,
    )
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
fn the_expected_fingerprint_is_compared_byte_for_byte() {
    let lowercase_certificate_fingerprint =
        compute_certificate_fingerprint(b"the first certificate");
    let uppercase_certificate_fingerprint = lowercase_certificate_fingerprint.to_uppercase();
    let fingerprint_verifier = FingerprintVerifier::from_expected_certificate_fingerprint(
        &uppercase_certificate_fingerprint,
    );

    let verification_error =
        verify_test_certificate(&fingerprint_verifier, b"the first certificate")
            .expect_err("an uppercase fingerprint does not match the lowercase one");

    assert_eq!(
        verification_error,
        rustls::Error::General(format!(
            "the expected certificate is {uppercase_certificate_fingerprint}, the server presented {lowercase_certificate_fingerprint}"
        ))
    );
    assert_eq!(
        fingerprint_verifier.get_presented_certificate_fingerprint(),
        Some(lowercase_certificate_fingerprint)
    );
}

#[test]
fn a_verifier_remembers_the_last_certificate_it_was_shown() {
    let fingerprint_verifier = FingerprintVerifier::from_expected_certificate_fingerprint(
        &compute_certificate_fingerprint(b"the second certificate"),
    );
    verify_test_certificate(&fingerprint_verifier, b"the first certificate")
        .expect_err("a certificate other than the expected one is refused");
    verify_test_certificate(&fingerprint_verifier, b"the second certificate")
        .expect("the expected certificate is taken");

    assert_eq!(
        fingerprint_verifier.get_presented_certificate_fingerprint(),
        Some(compute_certificate_fingerprint(b"the second certificate"))
    );
}
