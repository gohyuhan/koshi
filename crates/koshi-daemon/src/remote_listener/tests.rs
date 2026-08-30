//! Tests for the remote listener's connection-attempt table: how many
//! connections one address may open inside a window, that an address crossing
//! its limit is logged once rather than once per attempt, that a window ends
//! on its own, and that the table cannot grow past the count it is bounded at.
//!
//! Also [`Occasional`], which writes a repeated warning once per window,
//! [`EndReport`], which reports a bridged connection ended once, the two
//! functions that read and write one frame, the frames an admitted connection
//! sends, and what [`bind`] refuses.
//!
//! [`serve_remote`] is served over real TLS on loopback in two places: the
//! answers a caller reads before it is admitted, and one admitted client held
//! open while its session keeps emitting events.

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr};

use super::*;

use koshi_core::ids::SessionId;
use koshi_ipc::remote_state::CERT_FILE_FORMAT;

/// The address `10.0.0.<last>`, for naming distinct callers in a test.
fn caller(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
}

/// A self-signed certificate naming `koshi`, generated fresh for one test
/// listener.
fn test_cert() -> CertFile {
    let made = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .expect("the test certificate generates");
    CertFile {
        format: CERT_FILE_FORMAT,
        cert_der: made.cert.der().to_vec(),
        key_der: made.signing_key.serialize_der(),
    }
}

/// A moment `seconds` after `start`.
fn later(start: Instant, seconds: u64) -> Instant {
    start + Duration::from_secs(seconds)
}

#[test]
fn an_address_inside_its_limit_is_served_every_time() {
    let mut table = RateTable::new();
    let now = Instant::now();

    for attempt in 1..=MAX_ATTEMPTS {
        assert!(
            matches!(table.allow(caller(1), now), Attempt::Serve),
            "attempt {attempt} of {MAX_ATTEMPTS} is inside the limit"
        );
    }
}

#[test]
fn crossing_the_limit_is_logged_once_and_then_dropped_in_silence() {
    // One log line per address per window, not one per attempt.
    let mut table = RateTable::new();
    let now = Instant::now();

    for _ in 1..=MAX_ATTEMPTS {
        assert!(matches!(table.allow(caller(1), now), Attempt::Serve));
    }

    assert!(
        matches!(table.allow(caller(1), now), Attempt::DropAndSay),
        "the attempt that crosses the limit is the one that says so"
    );
    for attempt in 0..50 {
        assert!(
            matches!(table.allow(caller(1), now), Attempt::DropInSilence),
            "attempt {attempt} past the limit is dropped without a log line"
        );
    }
}

#[test]
fn a_window_that_has_passed_lets_the_same_address_back_in() {
    let mut table = RateTable::new();
    let opened = Instant::now();

    for _ in 1..=MAX_ATTEMPTS {
        assert!(matches!(table.allow(caller(1), opened), Attempt::Serve));
    }
    assert!(matches!(
        table.allow(caller(1), opened),
        Attempt::DropAndSay
    ));

    // One second before the window ends the address is still shut out.
    let nearly = later(opened, RATE_WINDOW.as_secs() - 1);
    assert!(matches!(
        table.allow(caller(1), nearly),
        Attempt::DropInSilence
    ));

    // Once the window has passed the address starts a fresh one.
    let after = later(opened, RATE_WINDOW.as_secs() + 1);
    assert!(matches!(table.allow(caller(1), after), Attempt::Serve));
}

#[test]
fn each_address_is_counted_on_its_own() {
    let mut table = RateTable::new();
    let now = Instant::now();

    for _ in 1..=MAX_ATTEMPTS {
        assert!(matches!(table.allow(caller(1), now), Attempt::Serve));
    }
    assert!(matches!(table.allow(caller(1), now), Attempt::DropAndSay));

    // A second address has opened nothing, so its first attempt is served.
    assert!(matches!(table.allow(caller(2), now), Attempt::Serve));
}

#[test]
fn the_table_never_holds_more_addresses_than_it_is_bounded_at() {
    // Every address gets one attempt, so only the bound keeps the table down.
    let mut table = RateTable::new();
    let now = Instant::now();

    for step in 0..u32::try_from(MAX_ENTRIES).expect("the bound fits") + 500 {
        let address = IpAddr::V4(Ipv4Addr::from(step));
        table.allow(address, now);
        assert!(
            table.entries.len() <= MAX_ENTRIES,
            "the table holds {} addresses after {step} of them, past the {MAX_ENTRIES} bound",
            table.entries.len()
        );
    }
    assert_eq!(table.entries.len(), MAX_ENTRIES);
}

#[test]
fn a_full_table_drops_the_address_whose_window_opened_first() {
    let mut table = RateTable::new();
    let opened = Instant::now();

    // Fill the table, each address one second after the one before it, so the
    // first one in is plainly the oldest.
    for step in 0..u32::try_from(MAX_ENTRIES).expect("the bound fits") {
        let at = later(opened, u64::from(step) % (RATE_WINDOW.as_secs() - 1));
        table.allow(IpAddr::V4(Ipv4Addr::from(step)), at);
    }
    assert_eq!(table.entries.len(), MAX_ENTRIES);
    let oldest = table
        .entries
        .iter()
        .min_by_key(|(_, window)| window.opened)
        .map(|(address, _)| *address)
        .expect("a full table holds an oldest address");

    table.allow(caller(200), opened);

    assert!(
        !table.entries.contains_key(&oldest),
        "the address whose window opened first left to make room"
    );
    assert!(
        table.entries.contains_key(&caller(200)),
        "the address that arrived took the room it made"
    );
    assert_eq!(table.entries.len(), MAX_ENTRIES);
}

#[test]
fn a_repeated_warning_is_written_once_per_window() {
    // Shared by the attempt table, the admission window and the accept loop.
    let mut warning = Occasional::new();
    let opened = Instant::now();

    assert!(warning.due(opened), "the first refusal says so");
    for step in 0..50 {
        assert!(
            !warning.due(later(opened, step % LOG_WINDOW.as_secs())),
            "refusal {step} inside the same window is silent"
        );
    }

    let after = later(opened, LOG_WINDOW.as_secs() + 1);
    assert!(warning.due(after), "a limit still refusing says so again");
    assert!(!warning.due(after), "and then goes quiet again");
}

#[test]
fn a_warning_that_has_never_been_written_is_due_at_once() {
    // No line has been written, so there is no window to be inside of.
    let mut warning = Occasional::new();
    assert!(warning.due(Instant::now()));
}

#[test]
fn a_bridged_connection_is_reported_ended_once_however_many_directions_get_there() {
    // Both directions end the connection and either may end first.
    let (events_tx, events_rx) = mpsc::channel();
    let ended = EndReport::new(events_tx, 7);

    ended.once();
    ended.once();
    ended.once();

    let reported = events_rx.try_recv().expect("the first report arrives");
    let RouterEvent::Admission(AdmissionAsk::Ended { id }) = reported else {
        panic!("a bridged connection ending is an Ended admission");
    };
    assert_eq!(id, 7);
    assert!(
        events_rx.try_recv().is_err(),
        "and nothing follows it, however many directions reported"
    );
}

#[test]
fn a_connection_reported_ended_by_the_direction_that_finished_first_needs_no_second() {
    // One direction reporting is enough; the other may still be blocked.
    let (events_tx, events_rx) = mpsc::channel();
    let ended = Arc::new(EndReport::new(events_tx, 3));

    let inbound = Arc::clone(&ended);
    std::thread::spawn(move || inbound.once())
        .join()
        .expect("the direction that finished first reports");

    let reported = events_rx.recv().expect("the report arrives");
    let RouterEvent::Admission(AdmissionAsk::Ended { id }) = reported else {
        panic!("a bridged connection ending is an Ended admission");
    };
    assert_eq!(id, 3, "without waiting for the other direction");
}

#[test]
fn the_admission_window_holds_only_what_it_is_bounded_at() {
    // Every place inside the window is taken, and the next caller is turned
    // away.
    let counted = Arc::new(AtomicUsize::new(0));
    let mut inside: Vec<InAdmission> = Vec::new();

    for place in 0..MAX_IN_ADMISSION {
        let taken = InAdmission::enter(&counted)
            .unwrap_or_else(|| panic!("place {place} of {MAX_IN_ADMISSION} is free"));
        inside.push(taken);
    }
    assert_eq!(counted.load(Ordering::Acquire), MAX_IN_ADMISSION);
    assert!(
        InAdmission::enter(&counted).is_none(),
        "the caller arriving at a full window is turned away"
    );
}

#[test]
fn a_place_in_the_admission_window_is_given_back_however_the_caller_left() {
    let counted = Arc::new(AtomicUsize::new(0));
    let mut inside: Vec<InAdmission> = (0..MAX_IN_ADMISSION)
        .map(|_| InAdmission::enter(&counted).expect("the window starts empty"))
        .collect();
    assert!(InAdmission::enter(&counted).is_none());

    // One caller leaves: admitted, refused and hung up are the same drop.
    inside.pop();
    assert_eq!(counted.load(Ordering::Acquire), MAX_IN_ADMISSION - 1);

    let next = InAdmission::enter(&counted).expect("the place it left is free");
    assert_eq!(counted.load(Ordering::Acquire), MAX_IN_ADMISSION);

    drop(next);
    drop(inside);
    assert_eq!(
        counted.load(Ordering::Acquire),
        0,
        "an empty window counts nothing"
    );
}

#[test]
fn an_address_whose_window_is_exactly_over_starts_a_fresh_one() {
    // The window ends at RATE_WINDOW, not one moment after it: an attempt
    // arriving at exactly that reading opens a new window and is served.
    let mut table = RateTable::new();
    let opened = Instant::now();
    for _ in 1..=MAX_ATTEMPTS {
        assert!(matches!(table.allow(caller(1), opened), Attempt::Serve));
    }
    assert!(matches!(
        table.allow(caller(1), opened),
        Attempt::DropAndSay
    ));

    assert!(
        matches!(
            table.allow(caller(1), opened + RATE_WINDOW - Duration::from_millis(1)),
            Attempt::DropInSilence
        ),
        "one millisecond before the window ends the address is still shut out"
    );
    assert!(
        matches!(table.allow(caller(1), opened + RATE_WINDOW), Attempt::Serve),
        "at exactly {RATE_WINDOW:?} the window is over"
    );
}

#[test]
fn a_warning_written_exactly_one_window_ago_is_written_again() {
    // The quiet spell ends at LOG_WINDOW, not one moment after it.
    let opened = Instant::now();

    let mut inside = Occasional::new();
    assert!(inside.due(opened));
    assert!(
        !inside.due(opened + LOG_WINDOW - Duration::from_millis(1)),
        "one millisecond before the window ends the warning is still silent"
    );

    let mut over = Occasional::new();
    assert!(over.due(opened));
    assert!(
        over.due(opened + LOG_WINDOW),
        "at exactly {LOG_WINDOW:?} the warning is written again"
    );
}

/// One frame's bytes as a caller sends them: a 4-byte big-endian length, then
/// the JSON.
fn framed(frame: &RemoteClientFrame) -> Vec<u8> {
    let payload = serde_json::to_vec(frame).expect("the frame encodes");
    let length = u32::try_from(payload.len()).expect("a test frame fits in a length prefix");
    let mut bytes = length.to_be_bytes().to_vec();
    bytes.extend_from_slice(&payload);
    bytes
}

#[test]
fn a_frame_the_length_of_the_cap_is_read_and_one_byte_over_it_is_not() {
    // The cap is what keeps a caller from naming a payload larger than this
    // machine will hold. A frame exactly at it is a caller inside the rule.
    let sent = RemoteClientFrame::Attach {
        session: SessionSelector::Name("S-quiet-lake".to_string()),
    };
    let bytes = framed(&sent);
    let payload_len = u32::try_from(bytes.len() - 4).expect("the payload fits");

    let mut exact = Cursor::new(bytes.clone());
    let Opening::Frame(read) = read_client_frame(&mut exact, payload_len) else {
        panic!("a frame the length of the cap is read");
    };
    assert_eq!(read, sent);
    assert_eq!(
        exact.position(),
        u64::try_from(bytes.len()).expect("the frame fits"),
        "and the whole frame was taken off the stream"
    );

    let mut over = Cursor::new(bytes);
    assert!(
        matches!(
            read_client_frame(&mut over, payload_len - 1),
            Opening::Closed
        ),
        "a length one byte over the cap closes the connection"
    );
    assert_eq!(over.position(), 4, "and its payload is never read");
}

#[test]
fn json_this_build_cannot_read_is_refused_and_a_stream_that_ends_early_is_not() {
    // The two answers are not the same: unreadable bytes get a refusal written
    // back, and a stream that ended has nobody left to write to.
    let junk = br#"{"Nonsense":1}"#.to_vec();
    let mut readable_frame = u32::try_from(junk.len())
        .expect("the junk fits")
        .to_be_bytes()
        .to_vec();
    readable_frame.extend_from_slice(&junk);
    assert!(
        matches!(
            read_client_frame(&mut Cursor::new(readable_frame), REMOTE_HELLO_MAX_LEN),
            Opening::Unreadable
        ),
        "a whole frame carrying JSON this build has no frame for is refused"
    );

    // The length says ten bytes and three follow.
    let mut cut_payload = 10u32.to_be_bytes().to_vec();
    cut_payload.extend_from_slice(b"abc");
    assert!(
        matches!(
            read_client_frame(&mut Cursor::new(cut_payload), REMOTE_HELLO_MAX_LEN),
            Opening::Closed
        ),
        "a payload that ends early closes the connection"
    );

    assert!(
        matches!(
            read_client_frame(&mut Cursor::new(vec![0u8, 0, 1]), REMOTE_HELLO_MAX_LEN),
            Opening::Closed
        ),
        "and so does a length prefix that ends early"
    );
}

#[test]
fn a_frame_naming_no_payload_carries_no_frame_this_build_reads() {
    // A length of zero is inside every cap. The four length bytes come off the
    // stream and the empty payload is what fails to decode.
    let mut empty = Cursor::new(0u32.to_be_bytes().to_vec());

    assert!(
        matches!(
            read_client_frame(&mut empty, REMOTE_HELLO_MAX_LEN),
            Opening::Unreadable
        ),
        "a frame naming no payload is refused rather than read"
    );
    assert_eq!(empty.position(), 4, "and its four length bytes were taken");
}

#[test]
fn one_answer_goes_out_as_a_big_endian_length_and_then_its_json() {
    // The caller reads the length the same way round. A length written the
    // other way round names another number, and the caller waits for bytes
    // that never come.
    let frame = RemoteServerFrame::Refused {
        message: REMOTE_REFUSED.to_string(),
    };
    let payload = serde_json::to_vec(&frame).expect("the frame encodes");
    let length = u32::try_from(payload.len()).expect("the answer fits");
    let mut expected = length.to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);

    let mut written = Vec::new();
    send_frame(&mut written, &frame).expect("the answer is written");

    assert_eq!(written, expected);
    assert_ne!(
        written[..4],
        length.to_le_bytes(),
        "a {} byte payload names different bytes each way round",
        payload.len()
    );
}

/// A `Vec`-backed writing half standing in for the TLS one: bytes are
/// recorded, and a deadline is taken and ignored.
struct RecordedWriter(Vec<u8>);

impl Write for RecordedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Deadlined for RecordedWriter {
    fn set_deadline(&mut self, _at: Option<Instant>) {}
}

/// A writing half that takes one byte per call, standing in for a socket that
/// accepts a little at a time.
struct OneByteWriter(Vec<u8>);

impl Write for OneByteWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match buf.first() {
            Some(byte) => {
                self.0.push(*byte);
                Ok(1)
            }
            None => Ok(0),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writing half that answers every write with [`io::ErrorKind::BrokenPipe`]
/// and the text `the caller hung up`.
struct BrokenWriter;

impl Write for BrokenWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "the caller hung up",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn one_answer_reaches_a_writer_that_takes_one_byte_at_a_time_whole() {
    // Every byte of the frame goes out, however little the socket takes per
    // call.
    let frame = RemoteServerFrame::Welcome {
        remote_version: REMOTE_PROTOCOL_VERSION,
    };
    let payload = serde_json::to_vec(&frame).expect("the frame encodes");
    let length = u32::try_from(payload.len()).expect("the answer fits");
    let mut expected = length.to_be_bytes().to_vec();
    expected.extend_from_slice(&payload);

    let mut written = OneByteWriter(Vec::new());
    send_frame(&mut written, &frame).expect("the answer is written");

    assert_eq!(written.0, expected);
}

#[test]
fn an_answer_the_writer_refuses_reports_that_writers_failure() {
    let frame = RemoteServerFrame::Refused {
        message: REMOTE_REFUSED.to_string(),
    };

    let failed = send_frame(&mut BrokenWriter, &frame).expect_err("a refused write is reported");

    assert_eq!(failed.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(failed.to_string(), "the caller hung up");
}

/// The server frames `bytes` holds, each read as a 4-byte big-endian length
/// and then its JSON.
fn server_frames(mut bytes: &[u8]) -> Vec<RemoteServerFrame> {
    let mut frames = Vec::new();
    while !bytes.is_empty() {
        let (length, rest) = bytes.split_at(4);
        let len = u32::from_be_bytes(length.try_into().expect("a 4-byte length")) as usize;
        let (payload, rest) = rest.split_at(len);
        frames.push(serde_json::from_slice(payload).expect("a server frame decodes"));
        bytes = rest;
    }
    frames
}

#[test]
fn one_admitted_connection_lists_and_then_attaches() {
    // The frame loop answers a `List` and keeps reading, so the same
    // connection may look at the sessions and then attach to one.
    let session = SessionId::new();
    let endpoint = PathBuf::from("endpoint-of-the-session");
    let held_endpoint = endpoint.clone();
    let (admissions, asked) = mpsc::channel();
    let dispatcher = std::thread::spawn(move || loop {
        match asked.recv() {
            Ok(RouterEvent::Admission(AdmissionAsk::Rows { scope, reply })) => {
                assert_eq!(scope, TokenScope::HostWide);
                let _ = reply.send(vec![RemoteSessionRow {
                    id: session,
                    name: "S-quiet-lake".to_string(),
                }]);
            }
            Ok(RouterEvent::Admission(AdmissionAsk::Locate {
                scope,
                id,
                selector,
                reply,
            })) => {
                assert_eq!(scope, TokenScope::HostWide);
                assert_eq!(id, 7);
                assert_eq!(selector, SessionSelector::Id(session));
                let _ = reply.send(Some(held_endpoint.clone()));
            }
            _ => return,
        }
    });

    let mut request = framed(&RemoteClientFrame::List);
    request.extend(framed(&RemoteClientFrame::Attach {
        session: SessionSelector::Id(session),
    }));
    let mut reader = Cursor::new(request);
    let mut writer = RecordedWriter(Vec::new());
    let admitted = Admitted {
        scope: TokenScope::HostWide,
        id: 7,
    };

    let attached = admitted_frames(&mut reader, &mut writer, &admitted, &admissions);

    assert_eq!(attached, Some(endpoint));
    assert_eq!(
        server_frames(&writer.0),
        vec![RemoteServerFrame::Sessions {
            rows: vec![RemoteSessionRow {
                id: session,
                name: "S-quiet-lake".to_string(),
            }],
        }],
        "the list was answered and the attach wrote nothing itself"
    );
    drop(admissions);
    dispatcher.join().expect("the stand-in dispatcher ended");
}

#[test]
fn a_second_hello_on_an_admitted_connection_is_refused() {
    // The Hello belongs to admission; an admitted connection sending another
    // one is refused and the connection ends unattached.
    let (admissions, _asked) = mpsc::channel();
    let mut reader = Cursor::new(framed(&RemoteClientFrame::Hello {
        min_remote_version: 1,
        max_remote_version: 1,
        min_protocol_version: 1,
        max_protocol_version: 1,
        token: ConnectionToken::new("alreadyAdmitted"),
    }));
    let mut writer = RecordedWriter(Vec::new());
    let admitted = Admitted {
        scope: TokenScope::HostWide,
        id: 3,
    };

    let attached = admitted_frames(&mut reader, &mut writer, &admitted, &admissions);

    assert_eq!(attached, None);
    assert_eq!(
        server_frames(&writer.0),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        }],
    );
}

#[test]
fn an_attach_the_dispatcher_refuses_ends_the_connection_unattached() {
    // A session that does not exist and a session the scope does not cover
    // both reach this loop as the same `None`.
    let session = SessionId::new();
    let (admissions, asked) = mpsc::channel();
    let dispatcher = std::thread::spawn(move || {
        let Ok(RouterEvent::Admission(AdmissionAsk::Locate {
            selector, reply, ..
        })) = asked.recv()
        else {
            panic!("an attach asks the dispatcher where the session listens");
        };
        assert_eq!(selector, SessionSelector::Id(session));
        let _ = reply.send(None);
    });

    let mut reader = Cursor::new(framed(&RemoteClientFrame::Attach {
        session: SessionSelector::Id(session),
    }));
    let mut writer = RecordedWriter(Vec::new());
    let admitted = Admitted {
        scope: TokenScope::HostWide,
        id: 11,
    };

    let attached = admitted_frames(&mut reader, &mut writer, &admitted, &admissions);

    assert_eq!(attached, None);
    assert_eq!(
        server_frames(&writer.0),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        }],
    );
    dispatcher.join().expect("the stand-in dispatcher ended");
}

#[test]
fn bytes_an_admitted_connection_sends_that_are_not_a_frame_are_refused() {
    // The cap is larger after admission, and JSON this build has no frame for
    // still reads as a refusal rather than as a hang-up.
    let junk = br#"{"Nonsense":1}"#.to_vec();
    let mut request = u32::try_from(junk.len())
        .expect("the junk fits")
        .to_be_bytes()
        .to_vec();
    request.extend_from_slice(&junk);
    let (admissions, _asked) = mpsc::channel();
    let mut reader = Cursor::new(request);
    let mut writer = RecordedWriter(Vec::new());
    let admitted = Admitted {
        scope: TokenScope::HostWide,
        id: 5,
    };

    let attached = admitted_frames(&mut reader, &mut writer, &admitted, &admissions);

    assert_eq!(attached, None);
    assert_eq!(
        server_frames(&writer.0),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        }],
    );
}

#[test]
fn an_admitted_connection_that_hangs_up_is_answered_with_nothing() {
    let (admissions, _asked) = mpsc::channel();
    let mut reader = Cursor::new(Vec::new());
    let mut writer = RecordedWriter(Vec::new());
    let admitted = Admitted {
        scope: TokenScope::HostWide,
        id: 9,
    };

    let attached = admitted_frames(&mut reader, &mut writer, &admitted, &admissions);

    assert_eq!(attached, None);
    assert_eq!(
        writer.0,
        Vec::<u8>::new(),
        "a caller that has left is written nothing"
    );
}

#[test]
fn an_admitted_connection_ends_unanswered_when_the_dispatcher_is_gone() {
    // The dispatcher hung up before the list could be answered. The connection
    // finishes with nothing written.
    let (admissions, asked) = mpsc::channel::<RouterEvent>();
    drop(asked);
    let mut reader = Cursor::new(framed(&RemoteClientFrame::List));
    let mut writer = RecordedWriter(Vec::new());
    let admitted = Admitted {
        scope: TokenScope::HostWide,
        id: 4,
    };

    let attached = admitted_frames(&mut reader, &mut writer, &admitted, &admissions);

    assert_eq!(attached, None);
    assert_eq!(writer.0, Vec::<u8>::new());
}

#[test]
fn the_hello_the_router_sends_for_a_remote_caller_says_so() {
    let hello = bridged_hello(ConnectionToken::new("endpointSecret"), (1, 4));

    assert_eq!(
        hello,
        IpcRequest {
            request_id: 1,
            kind: IpcRequestKind::Hello {
                min_protocol_version: 1,
                max_protocol_version: 4,
                token: ConnectionToken::new("endpointSecret"),
                remote: true,
            },
        }
    );
}

#[test]
fn a_certificate_no_tls_configuration_accepts_takes_no_port() {
    let cert = CertFile {
        format: CERT_FILE_FORMAT,
        cert_der: vec![1, 2, 3],
        key_der: vec![4, 5, 6],
    };

    let Err(failed) = bind("127.0.0.1:0".to_string(), &cert) else {
        panic!("bytes that are not a certificate build no TLS configuration");
    };

    assert_eq!(failed.kind(), io::ErrorKind::Other);
}

#[test]
fn an_address_that_names_no_socket_takes_no_port() {
    let Err(failed) = bind("not-an-address".to_string(), &test_cert()) else {
        panic!("a string that is not an address binds nothing");
    };

    assert_eq!(failed.kind(), io::ErrorKind::InvalidInput);
}

mod doorway {
    //! What [`serve_remote`] answers a caller before it is admitted, read by a
    //! real client over real TLS on loopback: a caller speaking no doorway
    //! version this build speaks, a secret the dispatcher refuses, an opening
    //! frame that is not a Hello, and a caller the dispatcher admits.

    use super::*;

    use koshi_ipc::protocol::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
    use koshi_ipc::remote_wire;

    /// How long the client gives the whole dial: the connect, the TLS
    /// handshake, the opening frame and the one frame answering it.
    const DIAL_WAIT: Duration = Duration::from_secs(10);

    /// Serve exactly one TLS connection on loopback with the real
    /// [`serve_remote`], answering its questions the way the router does.
    ///
    /// `admits` says what the stand-in dispatcher answers an
    /// [`AdmissionAsk::Admit`] with: a host-wide scope numbered 7 when it is
    /// true, and a refusal when it is false. Every locate is refused.
    ///
    /// Returns the address the client dials.
    fn start_doorway(admits: bool) -> String {
        let tls = Arc::new(server_config(&test_cert()).expect("the TLS config builds"));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test listener");
        let address = listener
            .local_addr()
            .expect("read the bound address")
            .to_string();

        let (admissions_tx, admissions_rx) = mpsc::channel::<RouterEvent>();
        std::thread::spawn(move || {
            while let Ok(RouterEvent::Admission(ask)) = admissions_rx.recv() {
                match ask {
                    AdmissionAsk::Admit { reply, .. } => {
                        let _ = reply.send(admits.then_some(Admitted {
                            scope: TokenScope::HostWide,
                            id: 7,
                        }));
                    }
                    AdmissionAsk::Rows { reply, .. } => {
                        let _ = reply.send(Vec::new());
                    }
                    AdmissionAsk::Locate { reply, .. } => {
                        let _ = reply.send(None);
                    }
                    AdmissionAsk::Ended { .. } => {}
                }
            }
        });

        std::thread::spawn(move || {
            let (sock, _) = listener.accept().expect("the listener accepts the client");
            let counted =
                InAdmission::enter(&Arc::new(AtomicUsize::new(0))).expect("a fresh count admits");
            serve_remote(sock, &tls, &admissions_tx, counted);
        });

        address
    }

    /// Dial the doorway at `address`, send `opening`, and hand back the one
    /// frame it answers with. No certificate is pinned.
    fn ask_doorway(address: &str, opening: &RemoteClientFrame) -> RemoteServerFrame {
        let (_reader, _writer, _fingerprint, answer) =
            remote_wire::open(address, None, opening, DIAL_WAIT, None)
                .expect("the doorway answers the opening frame");
        answer
    }

    /// A Hello naming the doorway versions `min_remote` to `max_remote`, the
    /// session protocol versions this build speaks, and the secret `token`.
    fn hello(min_remote: u32, max_remote: u32, token: &str) -> RemoteClientFrame {
        RemoteClientFrame::Hello {
            min_remote_version: min_remote,
            max_remote_version: max_remote,
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            token: ConnectionToken::new(token),
        }
    }

    #[test]
    fn a_caller_speaking_no_doorway_version_this_build_speaks_is_told_both_ranges() {
        // This dispatcher refuses every secret, and the answer still names the
        // ranges: the version is settled before the secret is looked at.
        let address = start_doorway(false);

        let answer = ask_doorway(
            &address,
            &hello(
                REMOTE_PROTOCOL_VERSION + 1,
                REMOTE_PROTOCOL_VERSION + 2,
                "neverReadSecret",
            ),
        );

        assert_eq!(
            answer,
            RemoteServerFrame::Refused {
                message: version_refusal(REMOTE_PROTOCOL_VERSION + 1, REMOTE_PROTOCOL_VERSION + 2),
            }
        );
    }

    #[test]
    fn a_secret_the_dispatcher_refuses_reads_as_every_other_refusal_does() {
        let address = start_doorway(false);

        let answer = ask_doorway(
            &address,
            &hello(
                MIN_REMOTE_PROTOCOL_VERSION,
                REMOTE_PROTOCOL_VERSION,
                "wrongSecret",
            ),
        );

        assert_eq!(
            answer,
            RemoteServerFrame::Refused {
                message: REMOTE_REFUSED.to_string(),
            }
        );
    }

    #[test]
    fn an_opening_frame_that_is_not_a_hello_is_refused_with_no_secret_presented() {
        // This dispatcher admits every secret; a List arriving first is still
        // refused.
        let address = start_doorway(true);

        let answer = ask_doorway(&address, &RemoteClientFrame::List);

        assert_eq!(
            answer,
            RemoteServerFrame::Refused {
                message: REMOTE_REFUSED.to_string(),
            }
        );
    }

    #[test]
    fn an_admitted_caller_is_welcomed_with_the_doorway_version_both_ends_speak() {
        let address = start_doorway(true);

        let answer = ask_doorway(
            &address,
            &hello(
                MIN_REMOTE_PROTOCOL_VERSION,
                REMOTE_PROTOCOL_VERSION,
                "testSecret",
            ),
        );

        assert_eq!(
            answer,
            RemoteServerFrame::Welcome {
                remote_version: REMOTE_PROTOCOL_VERSION,
            }
        );
    }
}

mod bridge_round_trip {
    //! One remote client served by [`serve_remote`] over real TLS on loopback,
    //! bridged to a real session server, held open while the session keeps
    //! emitting events. Every event must reach the remote client promptly and
    //! the connection must stay up the whole time.

    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::SystemTime;

    use super::*;

    use koshi_core::command::{
        Command, CommandEnvelope, CommandResult, CommandSource, FocusTabArgs, NewTabArgs, TabTarget,
    };
    use koshi_core::event::Event;
    use koshi_core::geometry::Size;
    use koshi_core::ids::CommandId;
    use koshi_ipc::event::SessionEvent;
    use koshi_ipc::protocol::{EventFilterSpec, IpcResponse, IpcResult};
    use koshi_ipc::router::SessionSelector;
    use koshi_link::remote_client;
    use koshi_pty::backend::state::PtyBackend;
    use koshi_runtime::ipc_server::IpcServer;
    use koshi_runtime::placeholder::{
        NullSnapshotProvider, NullStorage, SnapshotProvider, Storage,
    };
    use koshi_runtime::runtime::event::RuntimeEvent;
    use koshi_runtime::server::Server;
    use koshi_test_support::fake_pty::FakePtyBackend;
    use koshi_test_support::fixtures::test_runtime_dir;
    use tempfile::TempDir;

    const WAIT: Duration = Duration::from_secs(20);
    const POLL: Duration = Duration::from_millis(10);
    const VIEWPORT: Size = Size { cols: 80, rows: 24 };

    /// How long the link is held open and driven.
    const HOLD: Duration = Duration::from_secs(4);

    /// How long one emitted event has to reach the remote client.
    const EVENT_WAIT: Duration = Duration::from_secs(3);

    /// One session server on its own thread, serving a real control socket in
    /// its own runtime directory over a fake PTY backend.
    struct RunningSession {
        dir: TempDir,
        id: SessionId,
        inbox_tx: mpsc::Sender<RuntimeEvent>,
        dispatcher: Option<JoinHandle<()>>,
    }

    impl RunningSession {
        fn start() -> RunningSession {
            let dir = test_runtime_dir();
            let id = SessionId::new();
            let pty = Arc::new(FakePtyBackend::new());
            let (inbox_tx, inbox_rx) = mpsc::channel();

            let serving_dir = dir.path().to_path_buf();
            let serving_tx = inbox_tx.clone();
            let dispatcher = std::thread::spawn(move || {
                serve_session(&serving_dir, id, pty, inbox_rx, serving_tx);
            });

            let session = RunningSession {
                dir,
                id,
                inbox_tx,
                dispatcher: Some(dispatcher),
            };
            let deadline = Instant::now() + WAIT;
            while EndpointFile::read(&EndpointFile::path(session.dir.path(), session.id)).is_err() {
                assert!(
                    Instant::now() < deadline,
                    "the session server never advertised its socket"
                );
                std::thread::sleep(POLL);
            }
            session
        }

        fn endpoint_path(&self) -> PathBuf {
            EndpointFile::path(self.dir.path(), self.id)
        }
    }

    impl Drop for RunningSession {
        fn drop(&mut self) {
            let _ = self.inbox_tx.send(RuntimeEvent::Quit);
            if let Some(handle) = self.dispatcher.take() {
                let _ = handle.join();
            }
        }
    }

    fn serve_session(
        runtime_dir: &Path,
        session_id: SessionId,
        pty: Arc<FakePtyBackend>,
        inbox_rx: mpsc::Receiver<RuntimeEvent>,
        inbox_tx: mpsc::Sender<RuntimeEvent>,
    ) {
        let backend: Arc<dyn PtyBackend> = pty;
        let snapshot_provider: Arc<dyn SnapshotProvider> = Arc::new(NullSnapshotProvider);
        let storage: Arc<dyn Storage> = Arc::new(NullStorage);
        let mut server = Server::new(
            backend,
            snapshot_provider,
            storage,
            inbox_rx,
            inbox_tx.clone(),
        );
        server.load_startup_config(None);
        server
            .bootstrap_session(
                session_id,
                "quiet-lake".to_string(),
                VIEWPORT,
                SystemTime::now(),
                None,
            )
            .expect("the session is seeded");

        let ipc_server = IpcServer::start(runtime_dir, session_id, inbox_tx, None)
            .expect("the control socket binds");
        server.attach_ipc_server(ipc_server);

        loop {
            let now = Instant::now();
            let event = match server.next_render_wakeup(now) {
                Some(timeout) => match server.inbox_rx().recv_timeout(timeout) {
                    Ok(event) => Some(event),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => match server.inbox_rx().recv() {
                    Ok(event) => Some(event),
                    Err(_) => break,
                },
            };
            let mut quit = false;
            if let Some(event) = event {
                quit |= server.handle_runtime_event(event).is_break();
            }
            while let Ok(event) = server.inbox_rx().try_recv() {
                quit |= server.handle_runtime_event(event).is_break();
            }
            server.resync_lagged();
            if server.poll_render(Instant::now()) {
                server.push_frames();
            }
            if quit || server.quit_requested() || !server.has_active_panes() {
                break;
            }
        }
        server.shutdown();
    }

    /// Serve one TLS connection on loopback with the real [`serve_remote`],
    /// answering its admission questions the way the router does: the secret
    /// is admitted host-wide and every locate answers `endpoint_path`.
    ///
    /// Returns the address the client dials.
    fn start_listener(endpoint_path: PathBuf) -> String {
        let tls = Arc::new(server_config(&test_cert()).expect("the TLS config builds"));

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test listener");
        let address = listener
            .local_addr()
            .expect("read the bound address")
            .to_string();

        let (admissions_tx, admissions_rx) = mpsc::channel::<RouterEvent>();
        std::thread::spawn(move || {
            while let Ok(event) = admissions_rx.recv() {
                let RouterEvent::Admission(ask) = event else {
                    continue;
                };
                match ask {
                    AdmissionAsk::Admit { reply, .. } => {
                        let _ = reply.send(Some(Admitted {
                            scope: TokenScope::HostWide,
                            id: 7,
                        }));
                    }
                    AdmissionAsk::Rows { reply, .. } => {
                        let _ = reply.send(Vec::new());
                    }
                    AdmissionAsk::Locate { reply, .. } => {
                        let _ = reply.send(Some(endpoint_path.clone()));
                    }
                    AdmissionAsk::Ended { .. } => {}
                }
            }
        });

        std::thread::spawn(move || {
            let (sock, _) = listener.accept().expect("the listener accepts the client");
            let counted =
                InAdmission::enter(&Arc::new(AtomicUsize::new(0))).expect("a fresh count admits");
            serve_remote(sock, &tls, &admissions_tx, counted);
        });

        address
    }

    /// Submit `command` to the session over its own control connection, the
    /// way `koshi <verb>` does from outside every pane.
    fn submit(session: &RunningSession, command: Command) -> CommandResult {
        let endpoint = EndpointFile::read(&session.endpoint_path())
            .expect("the session server advertises its socket");
        let mut connection =
            Connection::connect(&endpoint.socket).expect("the control socket answers");
        connection
            .send(&IpcRequest {
                request_id: 1,
                kind: IpcRequestKind::Hello {
                    min_protocol_version: agreed_min(),
                    max_protocol_version: agreed_max(),
                    token: endpoint.token.clone(),
                    remote: false,
                },
            })
            .expect("the server reads the Hello");
        let hello: IpcResponse = connection.recv().expect("the server answers the Hello");
        match hello.result {
            IpcResult::Hello {
                protocol_version, ..
            } => assert_eq!(protocol_version, agreed_max()),
            other => panic!("the Hello was answered with {other:?}"),
        }

        let envelope = CommandEnvelope::new(
            CommandId::new(),
            CommandSource::external_cli(Some(session.id), None),
            SystemTime::now(),
            command,
        );
        connection
            .send(&IpcRequest {
                request_id: 2,
                kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
            })
            .expect("the server reads the command");
        let reply: IpcResponse = connection.recv().expect("the server answers the command");
        match reply.result {
            IpcResult::CommandResult(result) => result,
            other => panic!("the command was answered with {other:?}"),
        }
    }

    fn agreed_min() -> u32 {
        koshi_ipc::protocol::MIN_PROTOCOL_VERSION
    }

    fn agreed_max() -> u32 {
        koshi_ipc::protocol::PROTOCOL_VERSION
    }

    #[test]
    fn a_bridged_client_keeps_receiving_events_while_the_link_is_held_open() {
        let session = RunningSession::start();
        let address = start_listener(session.endpoint_path());

        // Dial through the real TLS doorway and attach, the way
        // `koshi attach --remote` does.
        let link = remote_client::connect(
            &address,
            &ConnectionToken::new("testSecret"),
            None,
            Duration::from_secs(5),
            None,
        )
        .expect("the listener admits the client");
        let (mut reader, mut writer) =
            remote_client::attach_remote(link, SessionSelector::Id(session.id))
                .expect("the attach request is written");

        // The session server's own Hello answer arrives through the bridge.
        let hello: IpcResponse = reader.recv().expect("the session answers the Hello");
        match hello.result {
            IpcResult::Hello {
                protocol_version, ..
            } => assert_eq!(protocol_version, agreed_max()),
            other => panic!("the Hello was answered with {other:?}"),
        }

        writer
            .send(&IpcRequest {
                request_id: 2,
                kind: IpcRequestKind::Attach {
                    viewport: VIEWPORT,
                    filter: EventFilterSpec::All,
                    resume: None,
                    resume_token: None,
                    pane_area: None,
                },
            })
            .expect("the attach is written");
        let reply: IpcResponse = reader.recv().expect("the session answers the attach");
        let IpcResult::Attached { client_id, .. } = reply.result else {
            panic!("expected an attach reply, got {:?}", reply.result);
        };

        // A second tab, so each focus change below moves focus and emits a
        // critical event.
        let created = submit(&session, Command::NewTab(NewTabArgs::default()));
        assert!(
            matches!(created, CommandResult::Ok { .. }),
            "the second tab was refused: {created:?}"
        );

        // Drive the session for the whole hold, one focus change at a time,
        // and require each one's event on the remote connection promptly.
        let hold_until = Instant::now() + HOLD;
        let mut rounds: u32 = 0;
        while Instant::now() < hold_until {
            let result = submit(
                &session,
                Command::FocusTab(FocusTabArgs {
                    target: TabTarget::Next,
                    client: Some(client_id),
                }),
            );
            let focused_onto = match &result {
                CommandResult::Ok { emitted_events, .. } => match emitted_events.as_slice() {
                    [Event::TabFocused(payload), ..] => payload.tab_id,
                    other => panic!("expected a focus event, got {other:?}"),
                },
                other => panic!("the focus change was refused: {other:?}"),
            };

            // Read frames until this round's focus event arrives. Painted
            // frames and other structure events on the way are read past.
            let event_deadline = Instant::now() + EVENT_WAIT;
            loop {
                reader.set_deadline(Some(event_deadline));
                let event: SessionEvent = match reader.recv() {
                    Ok(event) => event,
                    Err(error) => panic!(
                        "round {rounds}: the remote stream gave no frame within \
                         {EVENT_WAIT:?}: {error}"
                    ),
                };
                match event {
                    SessionEvent::TabFocused { tab_id, .. } if tab_id == focused_onto => break,
                    _ => {}
                }
            }
            rounds += 1;
            std::thread::sleep(Duration::from_millis(200));
        }

        assert!(
            rounds >= 12,
            "the hold made only {rounds} rounds; the link was not exercised"
        );
    }
}
