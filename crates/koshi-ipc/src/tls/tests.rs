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
    open, RemoteClientFrame, RemoteServerFrame, RemoteSessionRow, MIN_REMOTE_PROTOCOL_VERSION,
    REMOTE_PROTOCOL_VERSION,
};
use crate::transport::{frame_halves, Deadlined};

/// How long a loopback handshake and the frames after it have to finish. Well
/// past what a loopback stream needs, so a slow machine does not fail the run.
const LOOPBACK_WAIT: Duration = Duration::from_secs(10);

/// The timeout the deadline tests give a dial.
const SHORT_TIMEOUT: Duration = Duration::from_millis(300);

/// How far past its timeout a dial may still return, so a busy machine that
/// takes a moment to schedule the returning thread does not fail the run. The
/// bound is what the test proves: the dial returns, rather than waiting on a
/// server that never answers.
const SLACK: Duration = Duration::from_secs(3);

/// The timeout the opening-exchange tests give a dial: room for a loopback
/// handshake on a busy machine, and far less than the drip and the pause that
/// follow it.
const OPENING_WINDOW: Duration = Duration::from_secs(1);

/// How long a server waits before the frame it sends after the answer. Past
/// [`OPENING_WINDOW`], so a read still holding the dial's deadline would fail.
const PAUSE_AFTER_THE_ANSWER: Duration = Duration::from_millis(1500);

/// How long the drip tests leave between the bytes they send.
const DRIP_GAP: Duration = Duration::from_millis(50);

/// How many bytes the drip tests send. At one byte every [`DRIP_GAP`] the drip
/// lasts far longer than [`SHORT_TIMEOUT`] or [`OPENING_WINDOW`] with [`SLACK`]
/// on top, so a peer that stretched its deadline by dripping would fail these
/// tests.
const DRIP_BYTES: usize = 200;

/// The head of a TLS record of `kind`, the version, and a payload of 256
/// bytes. The drip that follows never reaches that many, so the record is
/// never whole and the reader keeps wanting more.
fn record_head(kind: u8) -> [u8; 5] {
    [kind, 0x03, 0x03, 0x01, 0x00]
}

/// Send `head` and then one byte every [`DRIP_GAP`], stopping early when the
/// peer has closed the socket.
fn drip(sock: &mut TcpStream, head: [u8; 5]) {
    if sock.write_all(&head).is_err() {
        return;
    }
    for _ in 0..DRIP_BYTES {
        std::thread::sleep(DRIP_GAP);
        if sock.write_all(&[0]).is_err() {
            return;
        }
    }
}

/// The name a verifier is handed. Never checked: the fingerprint is what a
/// server is recognised by.
fn any_name() -> ServerName<'static> {
    ServerName::try_from("127.0.0.1".to_string()).expect("a loopback address is a server name")
}

/// Ask `verifier` about the certificate `der`.
fn present(verifier: &PinVerifier, der: &[u8]) -> Result<ServerCertVerified, rustls::Error> {
    verifier.verify_server_cert(
        &CertificateDer::from(der.to_vec()),
        &[],
        &any_name(),
        &[],
        UnixTime::since_unix_epoch(Duration::from_secs(1_700_000_000)),
    )
}

#[test]
fn the_fingerprint_is_the_sha256_as_sixty_four_lowercase_hex_characters() {
    assert_eq!(
        fingerprint(&[]),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn a_first_connection_takes_the_certificate_and_records_its_fingerprint() {
    let verifier = PinVerifier::new(None);
    assert_eq!(verifier.seen(), None);
    present(&verifier, b"a certificate").expect("a first connection takes any certificate");
    assert_eq!(verifier.seen(), Some(fingerprint(b"a certificate")));
}

#[test]
fn the_pinned_fingerprint_is_taken_and_every_other_one_is_refused() {
    let pinned = fingerprint(b"the first certificate");
    let verifier = PinVerifier::new(Some(&pinned));
    present(&verifier, b"the first certificate").expect("the pinned certificate is taken");
    assert_eq!(verifier.seen(), Some(pinned.clone()));

    let verifier = PinVerifier::new(Some(&pinned));
    let refused = present(&verifier, b"another certificate").expect_err("a changed certificate");
    assert_eq!(
        refused,
        rustls::Error::General(format!(
            "the pinned certificate is {pinned}, the server presented {}",
            fingerprint(b"another certificate")
        ))
    );
    assert_eq!(verifier.seen(), Some(fingerprint(b"another certificate")));
}

#[test]
fn a_port_nothing_listens_on_refuses_the_dial_and_names_the_way_to_open_it() {
    // The operating system picks a free port. The listener goes before the
    // dial, and nothing holds that port when the connection arrives.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");
    drop(listener);

    let failure = dial(&address.to_string(), None, LOOPBACK_WAIT)
        .expect_err("a port nothing listens on refuses the connection");

    let printed = failure.to_string();
    let IpcError::ConnectRefused { address: named } = failure else {
        panic!("a refused connection is its own failure, not a transport failure");
    };
    assert_eq!(named, address.to_string());
    assert_eq!(
        printed,
        format!(
            "{address} refused the connection: nothing is listening on that port. \
             if remote access is not enabled on that machine, run `koshi share grant` \
             there and answer yes to the offer to open the port"
        )
    );
}

#[test]
fn a_dial_with_no_time_left_times_out_before_it_connects() {
    // A zero timeout leaves no time after the name lookup. The dial ends
    // before it opens a socket, and no bytes reach the address.
    let address = "127.0.0.1:1";

    let failure =
        dial(address, None, Duration::ZERO).expect_err("a dial with no time left never connects");

    let printed = failure.to_string();
    let IpcError::ConnectTimedOut { address: named } = failure else {
        panic!("a connect with no time left is a timeout, not a transport failure");
    };
    assert_eq!(named, address);
    assert_eq!(
        printed,
        "connecting to 127.0.0.1:1 timed out: nothing answered. check that the machine is up, \
         the address and port are right, and the network path allows it"
    );
}

#[test]
fn a_failed_handshake_names_the_address_and_the_reason() {
    assert_eq!(
        IpcError::TlsHandshakeFailed {
            address: "laptop.local:7654".to_string(),
            detail: "the TLS handshake did not finish in time".to_string(),
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
    let address = listener.local_addr().expect("read the bound address");

    let started = Instant::now();
    let failure = dial(&address.to_string(), None, SHORT_TIMEOUT)
        .expect_err("a server that sends nothing never finishes the handshake");
    let waited = started.elapsed();

    let IpcError::TlsHandshakeFailed {
        address: named,
        detail,
    } = failure
    else {
        panic!("a handshake that ran out of time is a handshake failure");
    };
    assert_eq!(named, address.to_string());
    assert_eq!(detail, "the TLS handshake did not finish in time");
    assert!(
        waited < SHORT_TIMEOUT + SLACK,
        "the dial returned {waited:?} after it started, inside its {SHORT_TIMEOUT:?} timeout"
    );
}

#[test]
fn a_server_that_sends_one_byte_at_a_time_ends_the_dial_at_the_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");

    let server = std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        drip(&mut sock, record_head(0x16));
    });

    let started = Instant::now();
    let failure = dial(&address.to_string(), None, SHORT_TIMEOUT)
        .expect_err("a server that drips its bytes never finishes the handshake");
    let waited = started.elapsed();

    let IpcError::TlsHandshakeFailed {
        address: named,
        detail,
    } = failure
    else {
        panic!("a handshake that ran out of time is a handshake failure");
    };
    assert_eq!(named, address.to_string());
    assert_eq!(detail, "the TLS handshake did not finish in time");
    assert!(
        waited < SHORT_TIMEOUT + SLACK,
        "the dial returned {waited:?} after it started, inside its {SHORT_TIMEOUT:?} timeout, \
         though the server kept it fed with a byte every {DRIP_GAP:?}"
    );
    let _ = server.join();
}

/// A fresh self-signed certificate and a server configuration serving it with
/// `provider`.
fn server_with(provider: Arc<CryptoProvider>) -> (ServerConfig, Vec<u8>) {
    let made = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .expect("generate a self-signed certificate");
    let cert_der = made.cert.der().to_vec();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(made.signing_key.serialize_der()));
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports every default protocol version")
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert_der.clone())], key)
        .expect("a certificate and its key make a server configuration");
    (config, cert_der)
}

/// A fresh self-signed certificate and the TLS configuration serving it, the
/// way this machine's own certificate is served.
fn fresh_server() -> (ServerConfig, Vec<u8>) {
    server_with(crypto_provider())
}

/// A fresh self-signed certificate and a server configuration whose key
/// exchange list holds `X25519` alone.
fn classical_only_server() -> (ServerConfig, Vec<u8>) {
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    provider.kx_groups = vec![rustls::crypto::aws_lc_rs::kx_group::X25519];
    server_with(Arc::new(provider))
}

/// The one session a loopback server reports.
fn one_row() -> RemoteSessionRow {
    RemoteSessionRow {
        id: SessionId::new(),
        name: "quiet-lake".to_string(),
    }
}

/// Serve `config` on a loopback port, dial it, and report the key exchange
/// group the handshake settled on together with the fingerprint the client
/// was shown.
fn negotiated_key_exchange(config: ServerConfig) -> (Option<NamedGroup>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");

    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept the client");
        let conn = ServerConnection::new(Arc::new(config)).expect("a server connection");
        let mut conn = rustls::Connection::Server(conn);
        handshake(&mut conn, &mut sock, Instant::now() + LOOPBACK_WAIT)
            .expect("the loopback handshake finishes");
        conn.negotiated_key_exchange_group().map(|kx| kx.name())
    });

    let (_reader, _writer, presented) =
        dial(&address.to_string(), None, LOOPBACK_WAIT).expect("the dial opens");
    let negotiated = server.join().expect("the server thread finishes");
    (negotiated, presented)
}

/// Two koshi peers settle on `X25519MLKEM768`: the X25519 elliptic curve
/// combined with ML-KEM-768, the post-quantum key encapsulation mechanism.
/// koshi offers that group ahead of every classical one.
#[test]
fn a_loopback_handshake_settles_on_the_hybrid_post_quantum_key_exchange() {
    let (config, cert_der) = fresh_server();

    let (negotiated, presented) = negotiated_key_exchange(config);

    assert_eq!(negotiated, Some(NamedGroup::X25519MLKEM768));
    assert_eq!(presented, fingerprint(&cert_der));
}

/// A server whose key exchange list holds `X25519` alone accepts the dial,
/// and the handshake settles on `X25519` rather than failing.
#[test]
fn a_server_that_offers_only_classical_key_exchange_still_accepts_a_dial() {
    let (config, cert_der) = classical_only_server();

    let (negotiated, presented) = negotiated_key_exchange(config);

    assert_eq!(negotiated, Some(NamedGroup::X25519));
    assert_eq!(presented, fingerprint(&cert_der));
}

#[test]
fn frames_cross_a_loopback_stream_both_ways_and_the_client_pins_what_it_was_shown() {
    let (config, cert_der) = fresh_server();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");
    let row = one_row();
    let served = row.clone();

    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept the client");
        let conn = ServerConnection::new(Arc::new(config)).expect("a server connection");
        let mut conn = rustls::Connection::Server(conn);
        handshake(&mut conn, &mut sock, Instant::now() + LOOPBACK_WAIT)
            .expect("the loopback handshake finishes");
        let (reader, writer) = split_tls(conn, sock).expect("split the loopback stream");
        let (mut incoming, mut outgoing) = frame_halves(Box::new(reader), Box::new(writer));

        let opened: RemoteClientFrame = incoming.recv().expect("the client's opening frame");
        outgoing
            .send(&RemoteServerFrame::Welcome {
                remote_version: REMOTE_PROTOCOL_VERSION,
            })
            .expect("answer the opening frame");
        let asked: RemoteClientFrame = incoming.recv().expect("the client's next frame");
        outgoing
            .send(&RemoteServerFrame::Sessions { rows: vec![served] })
            .expect("answer the list");
        (opened, asked)
    });

    let (reader, writer, presented) =
        dial(&address.to_string(), None, LOOPBACK_WAIT).expect("the dial opens");
    assert_eq!(presented, fingerprint(&cert_der));
    let (mut incoming, mut outgoing) = frame_halves(Box::new(reader), Box::new(writer));

    let hello = RemoteClientFrame::Hello {
        min_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        max_remote_version: REMOTE_PROTOCOL_VERSION,
        min_protocol_version: 1,
        max_protocol_version: 1,
        token: ConnectionToken::new("the secret the operator handed out"),
    };
    outgoing.send(&hello).expect("send the opening frame");
    assert_eq!(
        incoming
            .recv::<RemoteServerFrame>()
            .expect("read the answer"),
        RemoteServerFrame::Welcome {
            remote_version: REMOTE_PROTOCOL_VERSION
        }
    );
    outgoing
        .send(&RemoteClientFrame::List)
        .expect("ask for the sessions");
    assert_eq!(
        incoming
            .recv::<RemoteServerFrame>()
            .expect("read the sessions"),
        RemoteServerFrame::Sessions { rows: vec![row] }
    );

    let (opened, asked) = server.join().expect("the server thread finished");
    assert_eq!(opened, hello);
    assert_eq!(asked, RemoteClientFrame::List);
}

#[test]
fn a_peer_that_drips_after_the_handshake_ends_a_read_at_the_readers_deadline() {
    let (config, _) = fresh_server();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");

    let server = std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let Ok(conn) = ServerConnection::new(Arc::new(config)) else {
            return;
        };
        let mut conn = rustls::Connection::Server(conn);
        if handshake(&mut conn, &mut sock, Instant::now() + LOOPBACK_WAIT).is_err() {
            return;
        }
        drip(&mut sock, record_head(0x17));
    });

    let (mut reader, writer, _presented) =
        dial(&address.to_string(), None, LOOPBACK_WAIT).expect("the dial opens");
    reader.set_deadline(Some(Instant::now() + SHORT_TIMEOUT));

    let started = Instant::now();
    let mut length = [0u8; 4];
    let failure = reader
        .read_exact(&mut length)
        .expect_err("a drip never fills a frame");
    let waited = started.elapsed();

    assert!(
        waited_out(&failure),
        "the read ended on the deadline, not on the bytes: {failure:?}"
    );
    assert!(
        waited < SHORT_TIMEOUT + SLACK,
        "the read returned {waited:?} after it started, inside its {SHORT_TIMEOUT:?} deadline, \
         though the peer kept it fed with a byte every {DRIP_GAP:?}"
    );
    // Both halves hold the socket, so both go before the drip sees it close.
    drop(reader);
    drop(writer);
    let _ = server.join();
}

#[test]
fn a_second_connection_presenting_another_certificate_is_refused_by_the_pinned_fingerprint() {
    let (config, cert_der) = fresh_server();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");

    let server = std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let Ok(conn) = ServerConnection::new(Arc::new(config)) else {
            return;
        };
        let mut conn = rustls::Connection::Server(conn);
        let _ = handshake(&mut conn, &mut sock, Instant::now() + LOOPBACK_WAIT);
    });

    // The fingerprint of a certificate this server does not hold.
    let pinned = fingerprint(b"a certificate from another machine");
    let failure = dial(&address.to_string(), Some(&pinned), LOOPBACK_WAIT)
        .expect_err("a changed certificate is refused");

    let IpcError::CertificateChanged {
        address: named,
        pinned: was,
        presented,
    } = failure
    else {
        panic!("a changed certificate is its own refusal, not a transport failure");
    };
    assert_eq!(named, address.to_string());
    assert_eq!(was, pinned);
    assert_eq!(presented, fingerprint(&cert_der));
    let _ = server.join();
}

/// The opening frame a dialling client sends.
fn an_opening_frame() -> RemoteClientFrame {
    RemoteClientFrame::Hello {
        min_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        max_remote_version: REMOTE_PROTOCOL_VERSION,
        min_protocol_version: 1,
        max_protocol_version: 1,
        token: ConnectionToken::new("the secret the operator handed out"),
    }
}

#[test]
fn a_server_that_drips_its_answer_ends_the_opening_exchange_at_the_deadline() {
    let (config, _) = fresh_server();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");

    let server = std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let Ok(conn) = ServerConnection::new(Arc::new(config)) else {
            return;
        };
        let mut conn = rustls::Connection::Server(conn);
        if handshake(&mut conn, &mut sock, Instant::now() + LOOPBACK_WAIT).is_err() {
            return;
        }
        // The Hello is never read, and the answer never arrives whole.
        drip(&mut sock, record_head(0x17));
    });

    let started = Instant::now();
    let failure = open(
        &address.to_string(),
        None,
        &an_opening_frame(),
        OPENING_WINDOW,
        None,
    )
    .expect_err("a drip never fills the answer");
    let waited = started.elapsed();

    let IpcError::Transport { detail } = failure else {
        panic!("an exchange that ran out of time is a transport failure");
    };
    assert!(
        waited < OPENING_WINDOW + SLACK,
        "the exchange returned {waited:?} after it started, inside its {OPENING_WINDOW:?} \
         timeout, though the server kept it fed with a byte every {DRIP_GAP:?}: {detail}"
    );
    let _ = server.join();
}

#[test]
fn a_caller_that_asked_to_wait_reads_however_long_the_server_takes() {
    let (config, _) = fresh_server();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");
    let row = one_row();
    let served = row.clone();

    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept the client");
        let conn = ServerConnection::new(Arc::new(config)).expect("a server connection");
        let mut conn = rustls::Connection::Server(conn);
        handshake(&mut conn, &mut sock, Instant::now() + LOOPBACK_WAIT)
            .expect("the loopback handshake finishes");
        let (reader, writer) = split_tls(conn, sock).expect("split the loopback stream");
        let (mut incoming, mut outgoing) = frame_halves(Box::new(reader), Box::new(writer));
        let opened: RemoteClientFrame = incoming.recv().expect("the client's opening frame");
        outgoing
            .send(&RemoteServerFrame::Welcome {
                remote_version: REMOTE_PROTOCOL_VERSION,
            })
            .expect("answer the opening frame");
        std::thread::sleep(PAUSE_AFTER_THE_ANSWER);
        outgoing
            .send(&RemoteServerFrame::Sessions { rows: vec![served] })
            .expect("send the frame after the pause");
        opened
    });

    let (mut incoming, _outgoing, _presented, answer) = open(
        &address.to_string(),
        None,
        &an_opening_frame(),
        OPENING_WINDOW,
        None,
    )
    .expect("the opening exchange finishes");
    assert_eq!(
        answer,
        RemoteServerFrame::Welcome {
            remote_version: REMOTE_PROTOCOL_VERSION
        }
    );
    assert_eq!(
        incoming
            .recv::<RemoteServerFrame>()
            .expect("read the frame the server sent after the pause"),
        RemoteServerFrame::Sessions { rows: vec![row] }
    );

    let opened = server.join().expect("the server thread finished");
    assert_eq!(opened, an_opening_frame());
}

#[test]
fn a_caller_that_asked_for_a_bounded_wait_stops_reading_at_it() {
    // What a one-shot command needs. The server admits the connection and then
    // says nothing more; without the bound the read never returns and the
    // command has nothing to print and no reason to stop.
    let (config, _) = fresh_server();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");

    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept the client");
        let conn = ServerConnection::new(Arc::new(config)).expect("a server connection");
        let mut conn = rustls::Connection::Server(conn);
        handshake(&mut conn, &mut sock, Instant::now() + LOOPBACK_WAIT)
            .expect("the loopback handshake finishes");
        let (reader, writer) = split_tls(conn, sock).expect("split the loopback stream");
        let (mut incoming, mut outgoing) = frame_halves(Box::new(reader), Box::new(writer));
        let _: RemoteClientFrame = incoming.recv().expect("the client's opening frame");
        outgoing
            .send(&RemoteServerFrame::Welcome {
                remote_version: REMOTE_PROTOCOL_VERSION,
            })
            .expect("answer the opening frame");
        // Admitted, and then nothing. Held open so the client is reading a
        // live connection rather than a closed one.
        std::thread::sleep(PAUSE_AFTER_THE_ANSWER * 4);
    });

    let bounded = PAUSE_AFTER_THE_ANSWER / 2;
    let (mut incoming, _outgoing, _presented, answer) = open(
        &address.to_string(),
        None,
        &an_opening_frame(),
        OPENING_WINDOW,
        Some(bounded),
    )
    .expect("the opening exchange finishes");
    assert_eq!(
        answer,
        RemoteServerFrame::Welcome {
            remote_version: REMOTE_PROTOCOL_VERSION
        }
    );

    let started = Instant::now();
    let failure = incoming
        .recv::<RemoteServerFrame>()
        .expect_err("a server that says nothing more is not waited for");
    let waited = started.elapsed();

    assert!(
        waited < PAUSE_AFTER_THE_ANSWER * 3,
        "the read ended on the bound it was given, taking {waited:?}"
    );
    let IpcError::Transport { .. } = failure else {
        panic!("the read ran out of time, and did not misread a frame or lose the peer: {failure}");
    };

    let _ = server.join();
}

#[test]
fn a_framed_half_keeps_the_deadline_it_was_dialled_with_and_can_be_told_to_drop_it() {
    // The seam this pins: `open` hands back boxed halves, and the deadline has
    // to survive that box and still be removable through it. A caller that
    // could not remove it would hold a clock over frames that arrive when a
    // person types; a caller whose deadline the box swallowed would wait for
    // good on a server that admits a connection and then says nothing.
    let (config, _) = fresh_server();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");
    let row = one_row();
    let served = row.clone();

    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept the client");
        let conn = ServerConnection::new(Arc::new(config)).expect("a server connection");
        let mut conn = rustls::Connection::Server(conn);
        handshake(&mut conn, &mut sock, Instant::now() + LOOPBACK_WAIT)
            .expect("the loopback handshake finishes");
        let (reader, writer) = split_tls(conn, sock).expect("split the loopback stream");
        let (mut incoming, mut outgoing) = frame_halves(Box::new(reader), Box::new(writer));
        let _: RemoteClientFrame = incoming.recv().expect("the client's opening frame");
        outgoing
            .send(&RemoteServerFrame::Welcome {
                remote_version: REMOTE_PROTOCOL_VERSION,
            })
            .expect("answer the opening frame");
        std::thread::sleep(PAUSE_AFTER_THE_ANSWER);
        outgoing
            .send(&RemoteServerFrame::Sessions { rows: vec![served] })
            .expect("send the frame after the pause");
    });

    let bounded = PAUSE_AFTER_THE_ANSWER / 2;
    let (mut incoming, mut outgoing, _presented, answer) = open(
        &address.to_string(),
        None,
        &an_opening_frame(),
        OPENING_WINDOW,
        Some(bounded),
    )
    .expect("the opening exchange finishes");
    assert_eq!(
        answer,
        RemoteServerFrame::Welcome {
            remote_version: REMOTE_PROTOCOL_VERSION
        }
    );

    // The deadline came through the box: the server is still pausing, so this
    // read gives up rather than waiting it out.
    let held = incoming
        .recv::<RemoteServerFrame>()
        .expect_err("the dialled deadline holds through the boxed half");
    let IpcError::Transport { .. } = held else {
        panic!("a read that ran out of time is a transport failure, not a lost peer: {held}");
    };

    // And it can be taken off through the box: the same server, the same
    // pause, and now the frame is waited for.
    incoming.set_deadline(None);
    outgoing.set_deadline(None);
    assert_eq!(
        incoming
            .recv::<RemoteServerFrame>()
            .expect("with no deadline the frame after the pause arrives"),
        RemoteServerFrame::Sessions { rows: vec![row] }
    );

    let _ = server.join();
}

/// How many bytes the burst test sends in one go. Far past what one socket
/// read can hand the decryption state at once, so a reader that drops the
/// rest of a socket read loses bytes here.
const BURST_BYTES: usize = 400 * 1024;

#[test]
fn a_burst_larger_than_one_socket_read_arrives_whole() {
    let (config, _cert) = fresh_server();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");

    let sent: Vec<u8> = (0..BURST_BYTES).map(|i| (i % 251) as u8).collect();
    let written = sent.clone();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept the client");
        let conn = ServerConnection::new(Arc::new(config)).expect("a server connection");
        let mut conn = rustls::Connection::Server(conn);
        handshake(&mut conn, &mut sock, Instant::now() + LOOPBACK_WAIT)
            .expect("the loopback handshake finishes");
        let (_reader, mut writer) = split_tls(conn, sock).expect("split the loopback stream");
        writer.write_all(&written).expect("the burst is written");
    });

    let (mut reader, _writer, _presented) =
        dial(&address.to_string(), None, LOOPBACK_WAIT).expect("the dial opens");
    reader.set_deadline(Some(Instant::now() + LOOPBACK_WAIT));
    let mut received = vec![0u8; BURST_BYTES];
    reader
        .read_exact(&mut received)
        .expect("every byte of the burst arrives");
    assert_eq!(received, sent, "the burst arrived changed");

    server.join().expect("the server thread finished");
}

#[test]
fn the_fingerprint_of_three_bytes_is_their_sha256() {
    assert_eq!(
        fingerprint(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn the_provider_offers_the_hybrid_group_first_and_the_classical_ones_after() {
    let offered: Vec<NamedGroup> = crypto_provider()
        .kx_groups
        .iter()
        .map(|group| group.name())
        .collect();

    assert_eq!(
        offered,
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
    let failure = dial("127.0.0.1", None, LOOPBACK_WAIT)
        .expect_err("an address with no port names nothing to dial");

    let printed = failure.to_string();
    let IpcError::Transport { detail } = failure else {
        panic!("a failed lookup is a transport failure: {failure}");
    };
    assert_eq!(
        detail,
        "127.0.0.1 could not be looked up: invalid socket address"
    );
    assert_eq!(
        printed,
        "ipc transport error: 127.0.0.1 could not be looked up: invalid socket address"
    );
}

#[test]
fn an_address_whose_port_is_not_a_number_is_a_lookup_failure() {
    let failure = dial("127.0.0.1:seven", None, LOOPBACK_WAIT)
        .expect_err("a port that is not a number names nothing to dial");

    let IpcError::Transport { detail } = failure else {
        panic!("a failed lookup is a transport failure: {failure}");
    };
    assert_eq!(
        detail,
        "127.0.0.1:seven could not be looked up: invalid port value"
    );
}

#[test]
fn a_server_that_hangs_up_during_the_handshake_is_a_handshake_failure() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");

    let server = std::thread::spawn(move || {
        // Accepted, and dropped before a byte is answered.
        let _ = listener.accept();
    });

    let failure = dial(&address.to_string(), None, LOOPBACK_WAIT)
        .expect_err("a server that hangs up never finishes the handshake");

    let IpcError::TlsHandshakeFailed {
        address: named,
        detail,
    } = failure
    else {
        panic!("a peer gone mid-handshake is a handshake failure, not a transport failure");
    };
    assert_eq!(named, address.to_string());
    // The words are the operating system's: end of file on one platform, a
    // reset connection on another.
    assert_ne!(detail, "the TLS handshake did not finish in time");
    let _ = server.join();
}

/// A connected loopback socket pair: the dialling end and the accepted end.
fn loopback_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");
    let dialled = TcpStream::connect(address).expect("connect to loopback");
    let (accepted, _) = listener.accept().expect("accept the dial");
    (dialled, accepted)
}

#[test]
fn no_deadline_leaves_the_socket_timeouts_as_they_are() {
    let (sock, _peer) = loopback_pair();
    sock.set_read_timeout(Some(Duration::from_secs(7)))
        .expect("set a read timeout");
    sock.set_write_timeout(Some(Duration::from_secs(9)))
        .expect("set a write timeout");

    set_timeouts_until(&sock, None).expect("no deadline is not a failure");

    assert_eq!(
        sock.read_timeout().expect("read the timeout"),
        Some(Duration::from_secs(7))
    );
    assert_eq!(
        sock.write_timeout().expect("read the timeout"),
        Some(Duration::from_secs(9))
    );
}

#[test]
fn a_deadline_already_reached_is_timed_out_and_the_socket_timeouts_stay_unset() {
    let (sock, _peer) = loopback_pair();

    let failure =
        set_timeouts_until(&sock, Some(Instant::now())).expect_err("no time left is a failure");

    assert_eq!(failure.kind(), io::ErrorKind::TimedOut);
    assert_eq!(failure.to_string(), "this step ran out of time");
    assert_eq!(sock.read_timeout().expect("read the timeout"), None);
    assert_eq!(sock.write_timeout().expect("read the timeout"), None);
}

#[test]
fn a_deadline_ahead_sets_both_socket_timeouts_to_the_time_left() {
    let (sock, _peer) = loopback_pair();

    set_timeouts_until(&sock, Some(Instant::now() + Duration::from_secs(60)))
        .expect("time left is not a failure");

    let read = sock
        .read_timeout()
        .expect("read the timeout")
        .expect("a read timeout is set");
    let write = sock
        .write_timeout()
        .expect("read the timeout")
        .expect("a write timeout is set");
    // The time left shrinks between the call and this look at it.
    assert!(
        read > Duration::from_secs(59) && read <= Duration::from_secs(60),
        "the read timeout is the time left: {read:?}"
    );
    assert!(
        write > Duration::from_secs(59) && write <= Duration::from_secs(60),
        "the write timeout is the time left: {write:?}"
    );
}

/// The client configuration [`dial`] builds, with a verifier that takes any
/// certificate.
fn client_config() -> ClientConfig {
    ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports every default protocol version")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifier::new(None)))
        .with_no_client_auth()
}

/// A TLS stream over a loopback socket pair with no handshake run: the
/// dialling end split into its halves, and the accepted end.
fn split_without_handshake() -> (TlsReader, TlsWriter, TcpStream) {
    let (dialled, accepted) = loopback_pair();
    let client =
        ClientConnection::new(Arc::new(client_config()), any_name()).expect("a client connection");
    let (reader, writer) =
        split_tls(rustls::Connection::Client(client), dialled).expect("split the stream");
    (reader, writer, accepted)
}

#[test]
fn giving_a_half_a_deadline_stores_it_and_leaves_the_socket_timeouts_alone() {
    let (mut reader, writer, _peer) = split_without_handshake();
    reader
        .sock
        .set_read_timeout(Some(Duration::from_secs(7)))
        .expect("set a read timeout");
    let at = Instant::now() + Duration::from_secs(60);

    reader.set_deadline(Some(at));

    assert_eq!(reader.deadline, Some(at));
    assert_eq!(
        reader.sock.read_timeout().expect("read the timeout"),
        Some(Duration::from_secs(7))
    );
    // Both handles name one socket.
    assert_eq!(
        writer.sock.read_timeout().expect("read the timeout"),
        Some(Duration::from_secs(7))
    );
}

#[test]
fn taking_the_deadline_away_clears_the_socket_timeouts_both_halves_share() {
    let (mut reader, writer, _peer) = split_without_handshake();
    reader.set_deadline(Some(Instant::now() + Duration::from_secs(60)));
    writer
        .sock
        .set_read_timeout(Some(Duration::from_secs(7)))
        .expect("set a read timeout");
    writer
        .sock
        .set_write_timeout(Some(Duration::from_secs(9)))
        .expect("set a write timeout");

    reader.set_deadline(None);

    assert_eq!(reader.deadline, None);
    assert_eq!(writer.sock.read_timeout().expect("read the timeout"), None);
    assert_eq!(writer.sock.write_timeout().expect("read the timeout"), None);
}

#[test]
fn a_reader_prints_its_socket_and_deadline_and_none_of_its_buffer() {
    let (reader, _writer, _peer) = split_without_handshake();

    let printed = format!("{reader:?}");

    assert!(
        printed.starts_with("TlsReader { sock: TcpStream {"),
        "{printed}"
    );
    assert!(printed.ends_with(", deadline: None, .. }"), "{printed}");
}

#[test]
fn a_handshake_whose_deadline_has_passed_is_timed_out_before_the_socket_is_touched() {
    let (mut dialled, peer) = loopback_pair();
    let client =
        ClientConnection::new(Arc::new(client_config()), any_name()).expect("a client connection");
    let mut conn = rustls::Connection::Client(client);

    let failure = handshake(&mut conn, &mut dialled, Instant::now())
        .expect_err("a deadline already reached ends the handshake");

    assert_eq!(failure.kind(), io::ErrorKind::TimedOut);
    assert_eq!(
        failure.to_string(),
        "the TLS handshake did not finish in time"
    );
    // Nothing was written: the peer's read finds no byte and ends on its own
    // timeout.
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set a read timeout");
    let mut byte = [0u8; 1];
    let nothing = (&peer)
        .read(&mut byte)
        .expect_err("no byte reached the peer");
    assert!(
        waited_out(&nothing),
        "the peer's read ended on its timeout, not on bytes: {nothing:?}"
    );
}

/// Serve `config` on a loopback port and run the handshake on the connection
/// that arrives, then hand the finished stream to `after`. Returns the
/// address to dial and the thread.
fn serve_after_handshake<T: Send + 'static>(
    config: ServerConfig,
    after: impl FnOnce(rustls::Connection, TcpStream) -> T + Send + 'static,
) -> (String, std::thread::JoinHandle<T>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept the client");
        let conn = ServerConnection::new(Arc::new(config)).expect("a server connection");
        let mut conn = rustls::Connection::Server(conn);
        handshake(&mut conn, &mut sock, Instant::now() + LOOPBACK_WAIT)
            .expect("the loopback handshake finishes");
        after(conn, sock)
    });
    (address.to_string(), server)
}

#[test]
fn a_peer_that_closes_the_stream_cleanly_reads_as_end_of_stream_every_time() {
    let (config, _cert) = fresh_server();
    let (address, server) = serve_after_handshake(config, |mut conn, mut sock| {
        conn.send_close_notify();
        while conn.wants_write() {
            conn.write_tls(&mut sock).expect("the close is written");
        }
    });

    let (mut reader, _writer, _presented) =
        dial(&address, None, LOOPBACK_WAIT).expect("the dial opens");
    let mut byte = [0u8; 1];

    assert_eq!(
        reader
            .read(&mut byte)
            .expect("a clean close is end of stream"),
        0
    );
    assert_eq!(reader.read(&mut byte).expect("and stays end of stream"), 0);
    server.join().expect("the server thread finished");
}

#[test]
fn a_peer_that_drops_the_socket_without_closing_the_stream_is_an_unexpected_eof_every_time() {
    let (config, _cert) = fresh_server();
    let (address, server) = serve_after_handshake(config, |_conn, sock| drop(sock));

    let (mut reader, _writer, _presented) =
        dial(&address, None, LOOPBACK_WAIT).expect("the dial opens");
    let mut byte = [0u8; 1];

    let cut = reader
        .read(&mut byte)
        .expect_err("a cut stream is not end of stream");
    assert_eq!(cut.kind(), io::ErrorKind::UnexpectedEof);
    let again = reader.read(&mut byte).expect_err("and stays cut");
    assert_eq!(again.kind(), io::ErrorKind::UnexpectedEof);
    server.join().expect("the server thread finished");
}

#[test]
fn bytes_that_do_not_decrypt_end_the_read_with_invalid_data() {
    let (config, _cert) = fresh_server();
    let (address, server) = serve_after_handshake(config, |_conn, mut sock| {
        // A record of application data whose 32 bytes were never encrypted.
        let mut record = vec![0x17, 0x03, 0x03, 0x00, 0x20];
        record.extend_from_slice(&[0u8; 32]);
        sock.write_all(&record).expect("the record is written");
    });

    let (mut reader, _writer, _presented) =
        dial(&address, None, LOOPBACK_WAIT).expect("the dial opens");
    let mut byte = [0u8; 1];

    let failure = reader
        .read(&mut byte)
        .expect_err("bytes that do not decrypt are not plaintext");
    assert_eq!(failure.kind(), io::ErrorKind::InvalidData);
    server.join().expect("the server thread finished");
}

/// How many bytes one plaintext write hands to rustls at most: its send
/// buffer limit, 64 KiB.
const ONE_WRITE_TAKES: usize = 64 * 1024;

#[test]
fn one_write_takes_at_most_sixty_four_kib_and_write_all_delivers_the_rest() {
    let (config, _cert) = fresh_server();
    let sent: Vec<u8> = (0..ONE_WRITE_TAKES + 1000)
        .map(|i| (i % 251) as u8)
        .collect();
    let expected_len = sent.len();
    let (address, server) = serve_after_handshake(config, move |conn, sock| {
        let (mut reader, _writer) = split_tls(conn, sock).expect("split the loopback stream");
        let mut received = vec![0u8; expected_len];
        reader
            .read_exact(&mut received)
            .expect("every byte arrives");
        received
    });

    let (_reader, mut writer, _presented) =
        dial(&address, None, LOOPBACK_WAIT).expect("the dial opens");
    let taken = writer
        .write(&sent)
        .expect("the first write is taken in part");
    assert_eq!(taken, ONE_WRITE_TAKES);
    writer
        .write_all(&sent[taken..])
        .expect("the rest is written");

    assert_eq!(server.join().expect("the server thread finished"), sent);
}

#[test]
fn a_write_after_a_write_that_ran_out_of_time_still_takes_bytes() {
    // The timed-out write leaves 64 KiB of encrypted bytes queued. The next
    // write drains them first, so it has room for its own plaintext.
    let (config, _cert) = fresh_server();
    let (address, server) = serve_after_handshake(config, |conn, sock| {
        let (mut reader, _writer) = split_tls(conn, sock).expect("split the loopback stream");
        let mut received = Vec::new();
        let _ = reader.read_to_end(&mut received);
        received.len()
    });

    let (reader, mut writer, _presented) =
        dial(&address, None, LOOPBACK_WAIT).expect("the dial opens");
    writer.set_deadline(Some(Instant::now()));
    let held = writer
        .write(&[0u8; ONE_WRITE_TAKES])
        .expect_err("no time left ends the write");
    assert_eq!(held.kind(), io::ErrorKind::TimedOut);

    writer.set_deadline(None);
    assert_eq!(
        writer.write(b"after").expect("with time again it is taken"),
        5
    );

    drop(writer);
    drop(reader);
    assert_eq!(
        server.join().expect("the server thread finished"),
        ONE_WRITE_TAKES + 5
    );
}

/// How many bytes the blocked-write test sends: far past what the loopback
/// socket buffers of any platform hold, so the write blocks on a peer that
/// does not read.
const UNREAD_BYTES: usize = 32 * 1024 * 1024;

#[test]
fn a_write_to_a_peer_that_does_not_read_ends_at_the_writers_deadline() {
    let (config, _cert) = fresh_server();
    let (given_up_tx, given_up_rx) = std::sync::mpsc::channel::<()>();
    let (address, server) = serve_after_handshake(config, move |_conn, sock| {
        // Reads nothing, and holds the socket open until the write gave up.
        let _ = given_up_rx.recv_timeout(LOOPBACK_WAIT * 3);
        drop(sock);
    });

    let (_reader, mut writer, _presented) =
        dial(&address, None, LOOPBACK_WAIT).expect("the dial opens");
    writer.set_deadline(Some(Instant::now() + SHORT_TIMEOUT));

    let started = Instant::now();
    let failure = writer
        .write_all(&vec![0u8; UNREAD_BYTES])
        .expect_err("a peer that does not read never takes the bytes");
    let waited = started.elapsed();
    let _ = given_up_tx.send(());

    assert!(
        waited_out(&failure),
        "the write ended on the deadline, not on the bytes: {failure:?}"
    );
    assert!(
        waited < SHORT_TIMEOUT + SLACK,
        "the write returned {waited:?} after it started, inside its {SHORT_TIMEOUT:?} deadline"
    );
    server.join().expect("the server thread finished");
}

#[test]
fn less_than_a_millisecond_left_counts_as_no_time_left() {
    let (sock, _peer) = loopback_pair();

    let failure = set_timeouts_until(&sock, Some(Instant::now() + Duration::from_micros(500)))
        .expect_err("less than a millisecond is no time left");

    assert_eq!(failure.kind(), io::ErrorKind::TimedOut);
    assert_eq!(failure.to_string(), "this step ran out of time");
    assert_eq!(sock.read_timeout().expect("read the timeout"), None);
    assert_eq!(sock.write_timeout().expect("read the timeout"), None);
}

#[test]
fn an_empty_buffer_reads_as_zero_bytes_before_the_socket_is_touched() {
    let (mut reader, _writer, _peer) = split_without_handshake();
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
    let (mut reader, _writer, peer) = split_without_handshake();
    reader.set_deadline(Some(Instant::now()));
    let mut byte = [0u8; 1];

    let failure = reader
        .read(&mut byte)
        .expect_err("no time left ends the read");

    assert_eq!(failure.kind(), io::ErrorKind::TimedOut);
    assert_eq!(failure.to_string(), "this step ran out of time");
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set a read timeout");
    let nothing = (&peer)
        .read(&mut byte)
        .expect_err("no byte reached the peer");
    assert!(
        waited_out(&nothing),
        "the peer's read ended on its timeout, not on bytes: {nothing:?}"
    );
}

#[test]
fn a_write_with_no_time_left_is_timed_out_before_the_socket_is_touched() {
    let (_reader, mut writer, peer) = split_without_handshake();
    writer.set_deadline(Some(Instant::now()));

    let failure = writer
        .write(b"never sent")
        .expect_err("no time left ends the write");

    assert_eq!(failure.kind(), io::ErrorKind::TimedOut);
    assert_eq!(failure.to_string(), "this step ran out of time");
    peer.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set a read timeout");
    let mut byte = [0u8; 1];
    let nothing = (&peer)
        .read(&mut byte)
        .expect_err("no byte reached the peer");
    assert!(
        waited_out(&nothing),
        "the peer's read ended on its timeout, not on bytes: {nothing:?}"
    );
}

#[test]
fn an_empty_write_takes_zero_bytes() {
    let (_reader, mut writer, _peer) = split_without_handshake();

    assert_eq!(
        writer.write(b"").expect("an empty write is not a failure"),
        0
    );
}

#[test]
fn the_deadline_trait_reaches_the_halves_own_deadline() {
    let (mut reader, mut writer, _peer) = split_without_handshake();
    let at = Instant::now() + Duration::from_secs(60);
    writer
        .sock
        .set_write_timeout(Some(Duration::from_secs(9)))
        .expect("set a write timeout");

    Deadlined::set_deadline(&mut reader, Some(at));
    Deadlined::set_deadline(&mut writer, None);

    assert_eq!(reader.deadline, Some(at));
    assert_eq!(writer.deadline, None);
    assert_eq!(writer.sock.write_timeout().expect("read the timeout"), None);
}

#[test]
fn a_peer_whose_bytes_are_not_a_handshake_ends_it_with_invalid_data() {
    let (mut dialled, mut peer) = loopback_pair();
    let client =
        ClientConnection::new(Arc::new(client_config()), any_name()).expect("a client connection");
    let mut conn = rustls::Connection::Client(client);
    peer.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
        .expect("the peer's bytes are written");

    let failure = handshake(&mut conn, &mut dialled, Instant::now() + LOOPBACK_WAIT)
        .expect_err("bytes that are not a handshake end it");

    assert_eq!(failure.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        failure.to_string(),
        "received corrupt message of type InvalidContentType"
    );
}

#[test]
fn a_server_whose_bytes_are_not_a_handshake_is_a_handshake_failure_carrying_its_words() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");

    let server = std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        // Held open until the client has read the answer and hung up.
        let mut rest = Vec::new();
        let _ = sock.read_to_end(&mut rest);
    });

    let failure = dial(&address.to_string(), None, LOOPBACK_WAIT)
        .expect_err("a server that does not speak TLS never finishes the handshake");

    let IpcError::TlsHandshakeFailed {
        address: named,
        detail,
    } = failure
    else {
        panic!("bytes that are not a handshake are a handshake failure: {failure}");
    };
    assert_eq!(named, address.to_string());
    assert_eq!(
        detail,
        "received corrupt message of type InvalidContentType"
    );
    let _ = server.join();
}

#[test]
fn the_pinned_fingerprint_is_compared_byte_for_byte() {
    let lowercase = fingerprint(b"the first certificate");
    let uppercase = lowercase.to_uppercase();
    let verifier = PinVerifier::new(Some(&uppercase));

    let refused = present(&verifier, b"the first certificate")
        .expect_err("an uppercase pin does not match the lowercase fingerprint");

    assert_eq!(
        refused,
        rustls::Error::General(format!(
            "the pinned certificate is {uppercase}, the server presented {lowercase}"
        ))
    );
    assert_eq!(verifier.seen(), Some(lowercase));
}

#[test]
fn a_verifier_remembers_the_last_certificate_it_was_shown() {
    let verifier = PinVerifier::new(None);
    present(&verifier, b"the first certificate").expect("a first connection takes any certificate");
    present(&verifier, b"the second certificate").expect("and the next one too");

    assert_eq!(
        verifier.seen(),
        Some(fingerprint(b"the second certificate"))
    );
}
