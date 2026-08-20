//! Tests for the remote listener's connection-attempt table: how many
//! connections one address may open inside a window, that an address crossing
//! its limit is logged once rather than once per attempt, that a window ends
//! on its own, and that the table cannot grow past the count it is bounded at.
//!
//! Also [`Occasional`], which writes a repeated warning once per window,
//! [`EndReport`], which reports a bridged connection ended once, and the two
//! functions that read and write one frame.

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr};

use super::*;

use koshi_core::ids::SessionId;

/// The address `10.0.0.<last>`, for naming distinct callers in a test.
fn caller(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
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
