//! Tests for the TLS stream: the certificate fingerprint, what the pinning
//! verifier accepts and refuses, the frames that cross a real loopback stream,
//! and the deadline a handshake, an opening exchange and a read finish inside,
//! against a peer that answers nothing and against one that sends a byte at a
//! time.
//!
//! The loopback tests bind `127.0.0.1:0`, so the operating system picks a free
//! port and two runs of the suite never meet on one address.

use std::net::TcpListener;

use koshi_core::ids::SessionId;
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::{ServerConfig, ServerConnection};

use super::*;
use crate::protocol::ConnectionToken;
use crate::remote_wire::{
    open, RemoteClientFrame, RemoteServerFrame, RemoteSessionRow, MIN_REMOTE_PROTOCOL_VERSION,
    REMOTE_PROTOCOL_VERSION,
};
use crate::transport::frame_halves;

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
    assert!(verifier.seen().is_none());
    assert!(present(&verifier, b"a certificate").is_ok());
    assert_eq!(verifier.seen(), Some(fingerprint(b"a certificate")));
}

#[test]
fn the_pinned_fingerprint_is_taken_and_every_other_one_is_refused() {
    let pinned = fingerprint(b"the first certificate");
    let verifier = PinVerifier::new(Some(&pinned));
    assert!(present(&verifier, b"the first certificate").is_ok());

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
fn a_server_that_answers_nothing_ends_the_dial_at_the_deadline() {
    // Bound and never accepted: the operating system's backlog completes the
    // TCP connection, so the dial reaches the handshake and waits there.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let address = listener.local_addr().expect("read the bound address");

    let started = Instant::now();
    let failure = dial(&address.to_string(), None, SHORT_TIMEOUT)
        .expect_err("a server that sends nothing never finishes the handshake");
    let waited = started.elapsed();

    let IpcError::Transport { detail } = failure else {
        panic!("a dial that ran out of time is a transport failure");
    };
    assert_eq!(
        detail,
        format!(
            "the TLS handshake with {address} failed: the TLS handshake did not finish in time"
        )
    );
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

    let IpcError::Transport { detail } = failure else {
        panic!("a dial that ran out of time is a transport failure");
    };
    assert_eq!(
        detail,
        format!(
            "the TLS handshake with {address} failed: the TLS handshake did not finish in time"
        )
    );
    assert!(
        waited < SHORT_TIMEOUT + SLACK,
        "the dial returned {waited:?} after it started, inside its {SHORT_TIMEOUT:?} timeout, \
         though the server kept it fed with a byte every {DRIP_GAP:?}"
    );
    let _ = server.join();
}

/// A fresh self-signed certificate and the TLS configuration serving it, the
/// way this machine's own certificate is made.
fn fresh_server() -> (ServerConfig, Vec<u8>) {
    let made = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .expect("generate a self-signed certificate");
    let cert_der = made.cert.der().to_vec();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(made.signing_key.serialize_der()));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert_der.clone())], key)
        .expect("a certificate and its key make a server configuration");
    (config, cert_der)
}

/// The one session a loopback server reports.
fn one_row() -> RemoteSessionRow {
    RemoteSessionRow {
        id: SessionId::new(),
        name: "quiet-lake".to_string(),
    }
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
    assert_eq!(presented.len(), 64);
    assert_ne!(presented, pinned);
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
    assert!(
        !matches!(failure, IpcError::MalformedFrame { .. }),
        "the read ran out of time, and did not misread a frame: {failure}"
    );

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
    incoming
        .recv::<RemoteServerFrame>()
        .expect_err("the dialled deadline holds through the boxed half");

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
