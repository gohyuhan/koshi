//! Tests for the remote listener's connection-attempt table: how many
//! connections one address may open inside a window, that an address crossing
//! its limit is logged once rather than once per attempt, that a window ends
//! on its own, and that the table cannot grow past the count it is bounded at.
//!
//! Also [`WarningRateLimiter`], which writes a repeated warning once per window,
//! [`EndReport`], which reports a bridged connection ended once, the two
//! functions that read and write one frame, the frames an admitted connection
//! sends, and what [`bind_remote_listener`] refuses.
//!
//! [`serve_remote_connection`] is served over real TLS on loopback in two places: the
//! answers a caller reads before it is admitted, and one admitted client held
//! open while its session keeps emitting events.

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr};

use super::*;

use koshi_core::ids::SessionId;
use koshi_ipc::remote_state::CERT_FILE_FORMAT;

/// The address `10.0.0.<last_ipv4_octet>`, for naming distinct callers in a test.
fn build_test_caller_ip_address(last_ipv4_octet: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, last_ipv4_octet))
}

/// A self-signed certificate naming `koshi`, generated fresh for one test
/// listener.
fn build_test_certificate() -> CertFile {
    let generated_certificate = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .expect("the test certificate generates");
    CertFile {
        file_format: CERT_FILE_FORMAT,
        cert_der: generated_certificate.cert.der().to_vec(),
        key_der: generated_certificate.signing_key.serialize_der(),
    }
}

/// A moment `offset_seconds` after `start_time`.
fn add_seconds_to_instant(start_time: Instant, offset_seconds: u64) -> Instant {
    start_time + Duration::from_secs(offset_seconds)
}

#[test]
fn an_address_inside_its_limit_is_served_every_time() {
    let mut table = RateTable::new();
    let current_time = Instant::now();

    for attempt_index in 1..=MAX_ATTEMPT_COUNT {
        assert!(
            matches!(
                table.decide_attempt(build_test_caller_ip_address(1), current_time),
                Attempt::Serve
            ),
            "attempt {attempt_index} of {MAX_ATTEMPT_COUNT} is inside the limit"
        );
    }
}

#[test]
fn crossing_the_limit_is_logged_once_and_then_dropped_in_silence() {
    // One log line per address per window, not one per attempt.
    let mut table = RateTable::new();
    let current_time = Instant::now();

    for _ in 1..=MAX_ATTEMPT_COUNT {
        assert!(matches!(
            table.decide_attempt(build_test_caller_ip_address(1), current_time),
            Attempt::Serve
        ));
    }

    assert!(
        matches!(
            table.decide_attempt(build_test_caller_ip_address(1), current_time),
            Attempt::DropAndSay
        ),
        "the attempt that crosses the limit is the one that says so"
    );
    for refusal_index in 0..50 {
        assert!(
            matches!(
                table.decide_attempt(build_test_caller_ip_address(1), current_time),
                Attempt::DropInSilence
            ),
            "attempt {refusal_index} past the limit is dropped without a log line"
        );
    }
}

#[test]
fn a_window_that_has_passed_lets_the_same_address_back_in() {
    let mut table = RateTable::new();
    let window_started_at = Instant::now();

    for _ in 1..=MAX_ATTEMPT_COUNT {
        assert!(matches!(
            table.decide_attempt(build_test_caller_ip_address(1), window_started_at),
            Attempt::Serve
        ));
    }
    assert!(matches!(
        table.decide_attempt(build_test_caller_ip_address(1), window_started_at),
        Attempt::DropAndSay
    ));

    // One second before the window ends the address is still shut out.
    let before_window_end =
        add_seconds_to_instant(window_started_at, RATE_WINDOW_DURATION.as_secs() - 1);
    assert!(matches!(
        table.decide_attempt(build_test_caller_ip_address(1), before_window_end),
        Attempt::DropInSilence
    ));

    // Once the window has passed the address starts a fresh one.
    let after_window_end =
        add_seconds_to_instant(window_started_at, RATE_WINDOW_DURATION.as_secs() + 1);
    assert!(matches!(
        table.decide_attempt(build_test_caller_ip_address(1), after_window_end),
        Attempt::Serve
    ));
}

#[test]
fn each_address_is_counted_on_its_own() {
    let mut table = RateTable::new();
    let current_time = Instant::now();

    for _ in 1..=MAX_ATTEMPT_COUNT {
        assert!(matches!(
            table.decide_attempt(build_test_caller_ip_address(1), current_time),
            Attempt::Serve
        ));
    }
    assert!(matches!(
        table.decide_attempt(build_test_caller_ip_address(1), current_time),
        Attempt::DropAndSay
    ));

    // A second address has opened nothing, so its first attempt is served.
    assert!(matches!(
        table.decide_attempt(build_test_caller_ip_address(2), current_time),
        Attempt::Serve
    ));
}

#[test]
fn the_table_never_holds_more_addresses_than_it_is_bounded_at() {
    // Every address gets one attempt, so only the bound keeps the table down.
    let mut table = RateTable::new();
    let current_time = Instant::now();

    for address_index in 0..u32::try_from(MAX_RATE_TABLE_ENTRY_COUNT).expect("the bound fits") + 500
    {
        let peer_ip_address = IpAddr::V4(Ipv4Addr::from(address_index));
        table.decide_attempt(peer_ip_address, current_time);
        assert!(
            table.window_by_peer_ip_address.len() <= MAX_RATE_TABLE_ENTRY_COUNT,
            "the table holds {} addresses after {address_index} of them, past the {MAX_RATE_TABLE_ENTRY_COUNT} bound",
            table.window_by_peer_ip_address.len()
        );
    }
    assert_eq!(
        table.window_by_peer_ip_address.len(),
        MAX_RATE_TABLE_ENTRY_COUNT
    );
}

#[test]
fn a_full_table_drops_the_address_whose_window_opened_first() {
    let mut table = RateTable::new();
    let window_started_at = Instant::now();

    // Fill the table, each address one second after the one before it, so the
    // first one in is plainly the oldest.
    for address_index in 0..u32::try_from(MAX_RATE_TABLE_ENTRY_COUNT).expect("the bound fits") {
        let attempt_time = add_seconds_to_instant(
            window_started_at,
            u64::from(address_index) % (RATE_WINDOW_DURATION.as_secs() - 1),
        );
        table.decide_attempt(IpAddr::V4(Ipv4Addr::from(address_index)), attempt_time);
    }
    assert_eq!(
        table.window_by_peer_ip_address.len(),
        MAX_RATE_TABLE_ENTRY_COUNT
    );
    let oldest_peer_ip_address = table
        .window_by_peer_ip_address
        .iter()
        .min_by_key(|(_, peer_window)| peer_window.window_started_at)
        .map(|(peer_ip_address, _)| *peer_ip_address)
        .expect("a full table holds an oldest address");

    table.decide_attempt(build_test_caller_ip_address(200), window_started_at);

    assert!(
        !table
            .window_by_peer_ip_address
            .contains_key(&oldest_peer_ip_address),
        "the address whose window opened first left to make room"
    );
    assert!(
        table
            .window_by_peer_ip_address
            .contains_key(&build_test_caller_ip_address(200)),
        "the address that arrived took the room it made"
    );
    assert_eq!(
        table.window_by_peer_ip_address.len(),
        MAX_RATE_TABLE_ENTRY_COUNT
    );
}

#[test]
fn a_repeated_warning_is_written_once_per_window() {
    // Shared by the attempt table, the admission window and the accept loop.
    let mut repeated_warning = WarningRateLimiter::new();
    let window_started_at = Instant::now();

    assert!(
        repeated_warning.is_due(window_started_at),
        "the first refusal says so"
    );
    for refusal_index in 0..50 {
        assert!(
            !repeated_warning.is_due(add_seconds_to_instant(
                window_started_at,
                refusal_index % LOG_WINDOW_DURATION.as_secs(),
            )),
            "refusal {refusal_index} inside the same window is silent"
        );
    }

    let after_window_end =
        add_seconds_to_instant(window_started_at, LOG_WINDOW_DURATION.as_secs() + 1);
    assert!(
        repeated_warning.is_due(after_window_end),
        "a limit still refusing says so again"
    );
    assert!(
        !repeated_warning.is_due(after_window_end),
        "and then goes quiet again"
    );
}

#[test]
fn a_warning_that_has_never_been_written_is_due_at_once() {
    // No line has been written, so there is no window to be inside of.
    let mut warning = WarningRateLimiter::new();
    assert!(warning.is_due(Instant::now()));
}

#[test]
fn a_bridged_connection_is_reported_ended_once_however_many_directions_get_there() {
    // Both directions end the connection and either may end first.
    let (events_sender, events_receiver) = mpsc::channel();
    let end_report = EndReport::from_sender_and_connection_id(events_sender, 7);

    end_report.report_once();
    end_report.report_once();
    end_report.report_once();

    let end_report_event = events_receiver
        .try_recv()
        .expect("the first report arrives");
    let RouterEvent::Admission(AdmissionAsk::Ended {
        remote_connection_id,
    }) = end_report_event
    else {
        panic!("a bridged connection ending is an Ended admission");
    };
    assert_eq!(remote_connection_id, 7);
    assert!(
        events_receiver.try_recv().is_err(),
        "and nothing follows it, however many directions reported"
    );
}

#[test]
fn a_connection_reported_ended_by_the_direction_that_finished_first_needs_no_second() {
    // One direction reporting is enough; the other may still be blocked.
    let (events_sender, events_receiver) = mpsc::channel();
    let end_report = Arc::new(EndReport::from_sender_and_connection_id(events_sender, 3));

    let inbound_end_report = Arc::clone(&end_report);
    std::thread::spawn(move || inbound_end_report.report_once())
        .join()
        .expect("the direction that finished first reports");

    let end_report_event = events_receiver.recv().expect("the report arrives");
    let RouterEvent::Admission(AdmissionAsk::Ended {
        remote_connection_id,
    }) = end_report_event
    else {
        panic!("a bridged connection ending is an Ended admission");
    };
    assert_eq!(
        remote_connection_id, 3,
        "without waiting for the other direction"
    );
}

#[test]
fn concurrent_end_reports_send_one_admission() {
    let (events_sender, events_receiver) = mpsc::channel();
    let end_report = Arc::new(EndReport::from_sender_and_connection_id(events_sender, 8));
    let mut reporters = Vec::new();

    for _ in 0..8 {
        let end_report = Arc::clone(&end_report);
        reporters.push(std::thread::spawn(move || end_report.report_once()));
    }
    for reporter in reporters {
        reporter.join().expect("the report thread ends");
    }

    let end_report_events: Vec<_> = events_receiver.try_iter().collect();
    assert_eq!(
        end_report_events.len(),
        1,
        "concurrent directions report the end once"
    );
    let RouterEvent::Admission(AdmissionAsk::Ended {
        remote_connection_id,
    }) = &end_report_events[0]
    else {
        panic!("the report is an Ended admission");
    };
    assert_eq!(*remote_connection_id, 8);
}

#[test]
fn the_admission_window_holds_only_what_it_is_bounded_at() {
    // Every place inside the window is taken, and the next caller is turned
    // away.
    let admission_count = Arc::new(AtomicUsize::new(0));
    let mut admission_slots: Vec<AdmissionSlot> = Vec::new();

    for admission_slot_index in 0..MAX_ADMISSION_COUNT {
        let admission_slot =
            AdmissionSlot::enter_admission(&admission_count).unwrap_or_else(|| {
                panic!("place {admission_slot_index} of {MAX_ADMISSION_COUNT} is free")
            });
        admission_slots.push(admission_slot);
    }
    assert_eq!(admission_count.load(Ordering::Acquire), MAX_ADMISSION_COUNT);
    assert!(
        AdmissionSlot::enter_admission(&admission_count).is_none(),
        "the caller arriving at a full window is turned away"
    );
    assert_eq!(
        admission_count.load(Ordering::Acquire),
        MAX_ADMISSION_COUNT,
        "a refused caller does not change the number of occupied places"
    );
}

#[test]
fn a_place_in_the_admission_window_is_given_back_however_the_caller_left() {
    let admission_count = Arc::new(AtomicUsize::new(0));
    let mut admission_slots: Vec<AdmissionSlot> = (0..MAX_ADMISSION_COUNT)
        .map(|_| AdmissionSlot::enter_admission(&admission_count).expect("the window starts empty"))
        .collect();
    assert!(AdmissionSlot::enter_admission(&admission_count).is_none());

    // One caller leaves: admitted, refused and hung up are the same drop.
    admission_slots.pop();
    assert_eq!(
        admission_count.load(Ordering::Acquire),
        MAX_ADMISSION_COUNT - 1
    );

    let replacement_admission_slot =
        AdmissionSlot::enter_admission(&admission_count).expect("the place it left is free");
    assert_eq!(admission_count.load(Ordering::Acquire), MAX_ADMISSION_COUNT);

    drop(replacement_admission_slot);
    drop(admission_slots);
    assert_eq!(
        admission_count.load(Ordering::Acquire),
        0,
        "an empty window counts nothing"
    );
}

#[test]
fn an_address_whose_window_is_exactly_over_starts_a_fresh_one() {
    // The window ends at RATE_WINDOW_DURATION, not one moment after it: an attempt
    // arriving at exactly that reading opens a new window and is served.
    let mut table = RateTable::new();
    let window_started_at = Instant::now();
    for _ in 1..=MAX_ATTEMPT_COUNT {
        assert!(matches!(
            table.decide_attempt(build_test_caller_ip_address(1), window_started_at),
            Attempt::Serve
        ));
    }
    assert!(matches!(
        table.decide_attempt(build_test_caller_ip_address(1), window_started_at),
        Attempt::DropAndSay
    ));

    assert!(
        matches!(
            table.decide_attempt(
                build_test_caller_ip_address(1),
                window_started_at + RATE_WINDOW_DURATION - Duration::from_millis(1),
            ),
            Attempt::DropInSilence
        ),
        "one millisecond before the window ends the address is still shut out"
    );
    assert!(
        matches!(
            table.decide_attempt(
                build_test_caller_ip_address(1),
                window_started_at + RATE_WINDOW_DURATION,
            ),
            Attempt::Serve
        ),
        "at exactly {RATE_WINDOW_DURATION:?} the window is over"
    );
}

#[test]
fn a_warning_written_exactly_one_window_ago_is_written_again() {
    // The quiet spell ends at LOG_WINDOW_DURATION, not one moment after it.
    let window_started_at = Instant::now();

    let mut warning_within_window = WarningRateLimiter::new();
    assert!(warning_within_window.is_due(window_started_at));
    assert!(
        !warning_within_window
            .is_due(window_started_at + LOG_WINDOW_DURATION - Duration::from_millis(1)),
        "one millisecond before the window ends the warning is still silent"
    );

    let mut warning_at_window_end = WarningRateLimiter::new();
    assert!(warning_at_window_end.is_due(window_started_at));
    assert!(
        warning_at_window_end.is_due(window_started_at + LOG_WINDOW_DURATION),
        "at exactly {LOG_WINDOW_DURATION:?} the warning is written again"
    );
}

/// One frame's bytes as a caller sends them: a 4-byte big-endian length, then
/// the JSON.
fn build_remote_client_frame_bytes(remote_client_frame: &RemoteClientFrame) -> Vec<u8> {
    let payload_bytes = serde_json::to_vec(remote_client_frame).expect("the frame encodes");
    let frame_byte_count =
        u32::try_from(payload_bytes.len()).expect("a test frame fits in a length prefix");
    let mut frame_bytes = frame_byte_count.to_be_bytes().to_vec();
    frame_bytes.extend_from_slice(&payload_bytes);
    frame_bytes
}

#[test]
fn a_frame_the_length_of_the_cap_is_read_and_one_byte_over_it_is_not() {
    // The cap is what keeps a caller from naming a payload larger than this
    // machine will hold. A frame exactly at it is a caller inside the rule.
    let sent = RemoteClientFrame::Attach {
        session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
    };
    let frame_bytes = build_remote_client_frame_bytes(&sent);
    let payload_byte_count = u32::try_from(frame_bytes.len() - 4).expect("the payload fits");

    let mut exact = Cursor::new(frame_bytes.clone());
    let Opening::Frame(received_frame) = read_client_frame(&mut exact, payload_byte_count) else {
        panic!("a frame the length of the cap is read");
    };
    assert_eq!(received_frame, sent);
    assert_eq!(
        exact.position(),
        u64::try_from(frame_bytes.len()).expect("the frame fits"),
        "and the whole frame was taken off the stream"
    );

    let mut over_cap_stream = Cursor::new(frame_bytes);
    assert!(
        matches!(
            read_client_frame(&mut over_cap_stream, payload_byte_count - 1),
            Opening::Closed
        ),
        "a length one byte over the cap closes the connection"
    );
    assert_eq!(
        over_cap_stream.position(),
        4,
        "and its payload is never read"
    );
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
            read_client_frame(
                &mut Cursor::new(readable_frame),
                REMOTE_HELLO_MAX_BYTE_COUNT
            ),
            Opening::Unreadable
        ),
        "a whole frame carrying JSON this build has no frame for is refused"
    );

    // The length says ten bytes and three follow.
    let mut cut_payload = 10u32.to_be_bytes().to_vec();
    cut_payload.extend_from_slice(b"abc");
    assert!(
        matches!(
            read_client_frame(&mut Cursor::new(cut_payload), REMOTE_HELLO_MAX_BYTE_COUNT),
            Opening::Closed
        ),
        "a payload that ends early closes the connection"
    );

    assert!(
        matches!(
            read_client_frame(
                &mut Cursor::new(vec![0u8, 0, 1]),
                REMOTE_HELLO_MAX_BYTE_COUNT
            ),
            Opening::Closed
        ),
        "and so does a length prefix that ends early"
    );
}

#[test]
fn a_frame_naming_no_payload_carries_no_frame_this_build_reads() {
    // A length of zero is inside every cap. The four length bytes come off the
    // stream and the empty payload is what fails to decode.
    let mut empty_frame = Cursor::new(0u32.to_be_bytes().to_vec());

    assert!(
        matches!(
            read_client_frame(&mut empty_frame, REMOTE_HELLO_MAX_BYTE_COUNT),
            Opening::Unreadable
        ),
        "a frame naming no payload is refused rather than read"
    );
    assert_eq!(
        empty_frame.position(),
        4,
        "and its four length bytes were taken"
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
    let payload_bytes = serde_json::to_vec(&frame).expect("the frame encodes");
    let frame_byte_count = u32::try_from(payload_bytes.len()).expect("the answer fits");
    let mut expected_frame_bytes = frame_byte_count.to_be_bytes().to_vec();
    expected_frame_bytes.extend_from_slice(&payload_bytes);

    let mut written_frame_bytes = Vec::new();
    send_remote_frame(&mut written_frame_bytes, &frame).expect("the answer is written");

    assert_eq!(written_frame_bytes, expected_frame_bytes);
    assert_ne!(
        written_frame_bytes[..4],
        frame_byte_count.to_le_bytes(),
        "a {} byte payload names different bytes each way round",
        payload_bytes.len()
    );
}

/// A `Vec`-backed writing half standing in for the TLS one: bytes are
/// recorded, and a deadline is taken and ignored.
struct RecordedWriter {
    written_frame_bytes: Vec<u8>,
}

impl Write for RecordedWriter {
    fn write(&mut self, source_bytes: &[u8]) -> io::Result<usize> {
        self.written_frame_bytes.extend_from_slice(source_bytes);
        Ok(source_bytes.len())
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
struct OneByteWriter {
    written_frame_bytes: Vec<u8>,
}

impl Write for OneByteWriter {
    fn write(&mut self, source_bytes: &[u8]) -> io::Result<usize> {
        match source_bytes.first() {
            Some(byte) => {
                self.written_frame_bytes.push(*byte);
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
    fn write(&mut self, _source_bytes: &[u8]) -> io::Result<usize> {
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
        remote_protocol_version: REMOTE_PROTOCOL_VERSION,
    };
    let payload_bytes = serde_json::to_vec(&frame).expect("the frame encodes");
    let frame_byte_count = u32::try_from(payload_bytes.len()).expect("the answer fits");
    let mut expected_frame_bytes = frame_byte_count.to_be_bytes().to_vec();
    expected_frame_bytes.extend_from_slice(&payload_bytes);

    let mut written_frame_bytes = OneByteWriter {
        written_frame_bytes: Vec::new(),
    };
    send_remote_frame(&mut written_frame_bytes, &frame).expect("the answer is written");

    assert_eq!(
        written_frame_bytes.written_frame_bytes,
        expected_frame_bytes
    );
}

#[test]
fn an_answer_the_writer_refuses_reports_that_writers_failure() {
    let frame = RemoteServerFrame::Refused {
        message: REMOTE_REFUSED.to_string(),
    };

    let write_error =
        send_remote_frame(&mut BrokenWriter, &frame).expect_err("a refused write is reported");

    assert_eq!(write_error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(write_error.to_string(), "the caller hung up");
}

/// The server frames `frame_bytes` holds, each read as a 4-byte big-endian length
/// and then its JSON.
fn parse_remote_server_frames(mut frame_bytes: &[u8]) -> Vec<RemoteServerFrame> {
    let mut remote_server_frames = Vec::new();
    while !frame_bytes.is_empty() {
        let (length_bytes, remaining_frame_bytes) = frame_bytes.split_at(4);
        let payload_byte_count =
            u32::from_be_bytes(length_bytes.try_into().expect("a 4-byte length")) as usize;
        let (payload_bytes, remaining_frame_bytes) =
            remaining_frame_bytes.split_at(payload_byte_count);
        remote_server_frames
            .push(serde_json::from_slice(payload_bytes).expect("a server frame decodes"));
        frame_bytes = remaining_frame_bytes;
    }
    remote_server_frames
}

#[test]
fn one_admitted_connection_lists_and_then_attaches() {
    // The frame loop answers a `List` and keeps reading, so the same
    // connection may look at the sessions and then attach to one.
    let session_id = SessionId::new();
    let session_endpoint_path = PathBuf::from("endpoint-of-the-session");
    let held_session_endpoint_path = session_endpoint_path.clone();
    let (admission_events_sender, admission_question_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || loop {
        match admission_question_receiver.recv() {
            Ok(RouterEvent::Admission(AdmissionAsk::Rows {
                scope,
                response_sender,
            })) => {
                assert_eq!(scope, TokenScope::HostWide);
                let _ = response_sender.send(vec![RemoteSessionRow {
                    session_id,
                    session_name: "S-quiet-lake".to_string(),
                }]);
            }
            Ok(RouterEvent::Admission(AdmissionAsk::Locate {
                scope,
                remote_connection_id,
                session_selector,
                response_sender,
            })) => {
                assert_eq!(scope, TokenScope::HostWide);
                assert_eq!(remote_connection_id, 7);
                assert_eq!(session_selector, SessionSelector::SessionId(session_id));
                let _ = response_sender.send(Some(held_session_endpoint_path.clone()));
            }
            _ => return,
        }
    });

    let mut request_frame_bytes = build_remote_client_frame_bytes(&RemoteClientFrame::List);
    request_frame_bytes.extend(build_remote_client_frame_bytes(
        &RemoteClientFrame::Attach {
            session_selector: SessionSelector::SessionId(session_id),
        },
    ));
    let mut reader = Cursor::new(request_frame_bytes);
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = Admitted {
        scope: TokenScope::HostWide,
        remote_connection_id: 7,
    };

    let attached_session_endpoint_path = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &admission_events_sender,
    );

    assert_eq!(attached_session_endpoint_path, Some(session_endpoint_path));
    assert_eq!(
        parse_remote_server_frames(&writer.written_frame_bytes),
        vec![RemoteServerFrame::Sessions {
            session_rows: vec![RemoteSessionRow {
                session_id,
                session_name: "S-quiet-lake".to_string(),
            }],
        }],
        "the list was answered and the attach wrote nothing itself"
    );
    drop(admission_events_sender);
    dispatcher_thread
        .join()
        .expect("the stand-in dispatcher ended");
}

#[test]
fn a_second_hello_on_an_admitted_connection_is_refused() {
    // The Hello belongs to admission; an admitted connection sending another
    // one is refused and the connection ends unattached.
    let (admission_events_sender, _admission_question_receiver) = mpsc::channel();
    let mut reader = Cursor::new(build_remote_client_frame_bytes(&RemoteClientFrame::Hello {
        min_remote_version: 1,
        max_remote_version: 1,
        min_protocol_version: 1,
        max_protocol_version: 1,
        connection_token: ConnectionToken::from_secret("alreadyAdmitted"),
    }));
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = Admitted {
        scope: TokenScope::HostWide,
        remote_connection_id: 3,
    };

    let attached_session_endpoint_path = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &admission_events_sender,
    );

    assert_eq!(attached_session_endpoint_path, None);
    assert_eq!(
        parse_remote_server_frames(&writer.written_frame_bytes),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        }],
    );
}

#[test]
fn an_attach_the_dispatcher_refuses_ends_the_connection_unattached() {
    // A session that does not exist and a session the scope does not cover
    // both reach this loop as the same `None`.
    let session_id = SessionId::new();
    let (admission_events_sender, admission_question_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        let Ok(RouterEvent::Admission(AdmissionAsk::Locate {
            session_selector,
            response_sender,
            ..
        })) = admission_question_receiver.recv()
        else {
            panic!("an attach asks the dispatcher where the session listens");
        };
        assert_eq!(session_selector, SessionSelector::SessionId(session_id));
        let _ = response_sender.send(None);
    });

    let mut reader = Cursor::new(build_remote_client_frame_bytes(
        &RemoteClientFrame::Attach {
            session_selector: SessionSelector::SessionId(session_id),
        },
    ));
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = Admitted {
        scope: TokenScope::HostWide,
        remote_connection_id: 11,
    };

    let attached_session_endpoint_path = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &admission_events_sender,
    );

    assert_eq!(attached_session_endpoint_path, None);
    assert_eq!(
        parse_remote_server_frames(&writer.written_frame_bytes),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        }],
    );
    dispatcher_thread
        .join()
        .expect("the stand-in dispatcher ended");
}

#[test]
fn bytes_an_admitted_connection_sends_that_are_not_a_frame_are_refused() {
    // The cap is larger after admission, and JSON this build has no frame for
    // still reads as a refusal rather than as a hang-up.
    let junk = br#"{"Nonsense":1}"#.to_vec();
    let mut request_frame_bytes = u32::try_from(junk.len())
        .expect("the junk fits")
        .to_be_bytes()
        .to_vec();
    request_frame_bytes.extend_from_slice(&junk);
    let (admission_events_sender, _admission_question_receiver) = mpsc::channel();
    let mut reader = Cursor::new(request_frame_bytes);
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = Admitted {
        scope: TokenScope::HostWide,
        remote_connection_id: 5,
    };

    let attached_session_endpoint_path = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &admission_events_sender,
    );

    assert_eq!(attached_session_endpoint_path, None);
    assert_eq!(
        parse_remote_server_frames(&writer.written_frame_bytes),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        }],
    );
}

#[test]
fn an_admitted_connection_that_hangs_up_is_answered_with_nothing() {
    let (admission_events_sender, _admission_question_receiver) = mpsc::channel();
    let mut reader = Cursor::new(Vec::new());
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = Admitted {
        scope: TokenScope::HostWide,
        remote_connection_id: 9,
    };

    let attached_session_endpoint_path = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &admission_events_sender,
    );

    assert_eq!(attached_session_endpoint_path, None);
    assert_eq!(
        writer.written_frame_bytes,
        Vec::<u8>::new(),
        "a caller that has left is written nothing"
    );
}

#[test]
fn an_admitted_connection_ends_unanswered_when_the_dispatcher_is_gone() {
    // The dispatcher hung up before the list could be answered. The connection
    // finishes with nothing written.
    let (admission_events_sender, admission_question_receiver) = mpsc::channel::<RouterEvent>();
    drop(admission_question_receiver);
    let mut reader = Cursor::new(build_remote_client_frame_bytes(&RemoteClientFrame::List));
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = Admitted {
        scope: TokenScope::HostWide,
        remote_connection_id: 4,
    };

    let attached_session_endpoint_path = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &admission_events_sender,
    );

    assert_eq!(attached_session_endpoint_path, None);
    assert_eq!(writer.written_frame_bytes, Vec::<u8>::new());
}

#[test]
fn the_hello_the_router_sends_for_a_remote_caller_says_so() {
    let hello_request = build_bridged_hello(ConnectionToken::from_secret("endpointSecret"), (1, 4));

    assert_eq!(
        hello_request,
        IpcRequest {
            request_id: 1,
            request_kind: IpcRequestKind::Hello {
                min_protocol_version: 1,
                max_protocol_version: 4,
                connection_token: ConnectionToken::from_secret("endpointSecret"),
                is_remote: true,
            },
        }
    );
}

#[test]
fn a_certificate_no_tls_configuration_accepts_takes_no_port() {
    let certificate_file = CertFile {
        file_format: CERT_FILE_FORMAT,
        cert_der: vec![1, 2, 3],
        key_der: vec![4, 5, 6],
    };

    let Err(bind_error) = bind_remote_listener("127.0.0.1:0".to_string(), &certificate_file) else {
        panic!("bytes that are not a certificate build no TLS configuration");
    };

    assert_eq!(bind_error.kind(), io::ErrorKind::Other);
}

#[test]
fn an_address_that_names_no_socket_takes_no_port() {
    let Err(bind_error) =
        bind_remote_listener("not-an-address".to_string(), &build_test_certificate())
    else {
        panic!("a string that is not an address binds nothing");
    };

    assert_eq!(bind_error.kind(), io::ErrorKind::InvalidInput);
}

mod doorway {
    //! What [`serve_remote_connection`] answers a caller before it is admitted, read by a
    //! real client over real TLS on loopback: a caller speaking no doorway
    //! version this build speaks, a secret the dispatcher refuses, an opening
    //! frame that is not a Hello, and a caller the dispatcher admits.

    use super::*;

    use koshi_ipc::protocol::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
    use koshi_ipc::remote_wire;

    /// How long the client gives the whole dial: the connect, the TLS
    /// handshake, the opening frame and the one frame answering it.
    const REMOTE_DIAL_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

    /// Serve exactly one TLS connection on loopback with the real
    /// [`serve_remote_connection`], answering its questions the way the router does.
    ///
    /// `should_admit_connection` says what the stand-in dispatcher answers an
    /// [`AdmissionAsk::Admit`] with: a host-wide scope numbered 7 when it is
    /// true, and a refusal when it is false. Every locate is refused.
    ///
    /// Returns the address the client dials.
    fn start_test_remote_doorway(should_admit_connection: bool) -> String {
        let tls_config = Arc::new(
            build_server_config(&build_test_certificate()).expect("the TLS config builds"),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test listener");
        let remote_listen_address = listener
            .local_addr()
            .expect("read the bound address")
            .to_string();

        let (dispatcher_events_sender, dispatcher_events_receiver) = mpsc::channel::<RouterEvent>();
        std::thread::spawn(move || {
            while let Ok(RouterEvent::Admission(admission_request)) =
                dispatcher_events_receiver.recv()
            {
                match admission_request {
                    AdmissionAsk::Admit {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(should_admit_connection.then_some(Admitted {
                            scope: TokenScope::HostWide,
                            remote_connection_id: 7,
                        }));
                    }
                    AdmissionAsk::Rows {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(Vec::new());
                    }
                    AdmissionAsk::Locate {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(None);
                    }
                    AdmissionAsk::Ended { .. } => {}
                }
            }
        });

        std::thread::spawn(move || {
            let (accepted_socket, _) = listener.accept().expect("the listener accepts the client");
            let admission_slot = AdmissionSlot::enter_admission(&Arc::new(AtomicUsize::new(0)))
                .expect("a fresh count admits");
            serve_remote_connection(
                accepted_socket,
                &tls_config,
                &dispatcher_events_sender,
                admission_slot,
            );
        });

        remote_listen_address
    }

    /// Dial the doorway at `remote_listen_address`, send `opening_frame`, and hand back the one
    /// frame it answers with. No certificate is pinned.
    fn request_test_remote_doorway(
        remote_listen_address: &str,
        opening_frame: &RemoteClientFrame,
    ) -> RemoteServerFrame {
        let (_reader, _writer, _certificate_fingerprint, response_frame) =
            remote_wire::open_remote_connection(
                remote_listen_address,
                None,
                opening_frame,
                REMOTE_DIAL_TIMEOUT_DURATION,
                None,
            )
            .expect("the doorway answers the opening frame");
        response_frame
    }

    /// A Hello naming the doorway versions `minimum_remote_version` to `maximum_remote_version`, the
    /// session protocol versions this build speaks, and the secret `connection_token_text`.
    fn build_remote_hello(
        minimum_remote_version: u32,
        maximum_remote_version: u32,
        connection_token_text: &str,
    ) -> RemoteClientFrame {
        RemoteClientFrame::Hello {
            min_remote_version: minimum_remote_version,
            max_remote_version: maximum_remote_version,
            min_protocol_version: MIN_PROTOCOL_VERSION,
            max_protocol_version: PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret(connection_token_text),
        }
    }

    #[test]
    fn a_caller_speaking_no_doorway_version_this_build_speaks_is_told_both_ranges() {
        // This dispatcher refuses every secret, and the answer still names the
        // ranges: the version is settled before the secret is looked at.
        let remote_listen_address = start_test_remote_doorway(false);

        let response_frame = request_test_remote_doorway(
            &remote_listen_address,
            &build_remote_hello(
                REMOTE_PROTOCOL_VERSION + 1,
                REMOTE_PROTOCOL_VERSION + 2,
                "neverReadSecret",
            ),
        );

        assert_eq!(
            response_frame,
            RemoteServerFrame::Refused {
                message: format_version_refusal(
                    REMOTE_PROTOCOL_VERSION + 1,
                    REMOTE_PROTOCOL_VERSION + 2,
                ),
            }
        );
    }

    #[test]
    fn a_secret_the_dispatcher_refuses_reads_as_every_other_refusal_does() {
        let remote_listen_address = start_test_remote_doorway(false);

        let response_frame = request_test_remote_doorway(
            &remote_listen_address,
            &build_remote_hello(
                MIN_REMOTE_PROTOCOL_VERSION,
                REMOTE_PROTOCOL_VERSION,
                "wrongSecret",
            ),
        );

        assert_eq!(
            response_frame,
            RemoteServerFrame::Refused {
                message: REMOTE_REFUSED.to_string(),
            }
        );
    }

    #[test]
    fn an_opening_frame_that_is_not_a_hello_is_refused_with_no_secret_presented() {
        // This dispatcher admits every secret; a List arriving first is still
        // refused.
        let remote_listen_address = start_test_remote_doorway(true);

        let response_frame =
            request_test_remote_doorway(&remote_listen_address, &RemoteClientFrame::List);

        assert_eq!(
            response_frame,
            RemoteServerFrame::Refused {
                message: REMOTE_REFUSED.to_string(),
            }
        );
    }

    #[test]
    fn an_admitted_caller_is_welcomed_with_the_doorway_version_both_ends_speak() {
        let remote_listen_address = start_test_remote_doorway(true);

        let response_frame = request_test_remote_doorway(
            &remote_listen_address,
            &build_remote_hello(
                MIN_REMOTE_PROTOCOL_VERSION,
                REMOTE_PROTOCOL_VERSION,
                "testSecret",
            ),
        );

        assert_eq!(
            response_frame,
            RemoteServerFrame::Welcome {
                remote_protocol_version: REMOTE_PROTOCOL_VERSION,
            }
        );
    }
}

mod bridge_round_trip {
    //! One remote client served by [`serve_remote_connection`] over real TLS on loopback,
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
    use koshi_core::key::{Key, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags};
    use koshi_ipc::event::SessionEvent;
    use koshi_ipc::protocol::{EventFilterSpec, IpcResponse, IpcResult};
    use koshi_ipc::router::SessionSelector;
    use koshi_link::remote_client;
    use koshi_pty::backend::state::PtyBackend;
    use koshi_runtime::ipc_server::IpcServer;
    use koshi_runtime::runtime::event::RuntimeEvent;
    use koshi_runtime::server::Server;
    use koshi_test_support::fake_pty::FakePtyBackend;
    use koshi_test_support::fixtures::build_test_runtime_directory;
    use tempfile::TempDir;

    const SESSION_SERVER_START_TIMEOUT_DURATION: Duration = Duration::from_secs(20);
    const SESSION_SERVER_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(10);
    const TEST_VIEWPORT_SIZE: Size = Size {
        column_count: 80,
        row_count: 24,
    };

    /// How long the link is held open and driven.
    const LINK_HOLD_DURATION: Duration = Duration::from_secs(4);

    /// How long one emitted event has to reach the remote client.
    const EVENT_DELIVERY_TIMEOUT_DURATION: Duration = Duration::from_secs(3);

    /// How long the pane's writes must stop changing before they are read.
    const PANE_WRITE_SETTLE_DURATION: Duration = Duration::from_millis(300);

    /// One session server on its own thread, serving a real control socket in
    /// its own runtime directory over a fake PTY backend.
    struct RunningSession {
        runtime_directory: TempDir,
        session_id: SessionId,
        runtime_event_sender: mpsc::Sender<RuntimeEvent>,
        session_server_thread: Option<JoinHandle<()>>,
        fake_pty_backend: Arc<FakePtyBackend>,
    }

    impl RunningSession {
        fn start_running_session() -> RunningSession {
            let runtime_directory = build_test_runtime_directory();
            let session_id = SessionId::new();
            let fake_pty_backend = Arc::new(FakePtyBackend::new());
            let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();

            let session_server_runtime_directory = runtime_directory.path().to_path_buf();
            let session_server_event_sender = runtime_event_sender.clone();
            let session_server_pty_backend = Arc::clone(&fake_pty_backend);
            let session_server_thread = std::thread::spawn(move || {
                run_test_session_server(
                    &session_server_runtime_directory,
                    session_id,
                    session_server_pty_backend,
                    runtime_event_receiver,
                    session_server_event_sender,
                );
            });

            let running_session = RunningSession {
                runtime_directory,
                session_id,
                runtime_event_sender,
                session_server_thread: Some(session_server_thread),
                fake_pty_backend,
            };
            let session_server_start_deadline =
                Instant::now() + SESSION_SERVER_START_TIMEOUT_DURATION;
            while EndpointFile::load_from_path(&EndpointFile::resolve_endpoint_file_path(
                running_session.runtime_directory.path(),
                running_session.session_id,
            ))
            .is_err()
            {
                assert!(
                    Instant::now() < session_server_start_deadline,
                    "the session server never advertised its socket"
                );
                std::thread::sleep(SESSION_SERVER_POLL_INTERVAL_DURATION);
            }
            running_session
        }

        fn get_endpoint_path(&self) -> PathBuf {
            EndpointFile::resolve_endpoint_file_path(self.runtime_directory.path(), self.session_id)
        }

        /// The bytes the session's only pane has been written, once they stop
        /// changing for `PANE_WRITE_SETTLE_DURATION`.
        ///
        /// # Panics
        ///
        /// Panics when the session has not spawned its pane within
        /// [`SESSION_SERVER_START_TIMEOUT_DURATION`].
        fn wait_for_settled_pane_write_bytes(&self) -> Vec<Vec<u8>> {
            let spawn_deadline = Instant::now() + SESSION_SERVER_START_TIMEOUT_DURATION;
            let pane_id = loop {
                if let Some(pane_id) = self.fake_pty_backend.list_spawned_pane_ids().first() {
                    break *pane_id;
                }
                assert!(
                    Instant::now() < spawn_deadline,
                    "the session never spawned a pane"
                );
                std::thread::sleep(SESSION_SERVER_POLL_INTERVAL_DURATION);
            };
            let mut settled_write_bytes = Vec::new();
            loop {
                std::thread::sleep(PANE_WRITE_SETTLE_DURATION);
                let write_bytes = self
                    .fake_pty_backend
                    .list_pane_write_bytes(pane_id)
                    .expect("the pane is spawned");
                if write_bytes == settled_write_bytes {
                    return settled_write_bytes;
                }
                settled_write_bytes = write_bytes;
            }
        }
    }

    impl Drop for RunningSession {
        fn drop(&mut self) {
            let _ = self.runtime_event_sender.send(RuntimeEvent::Quit);
            if let Some(session_server_thread) = self.session_server_thread.take() {
                let _ = session_server_thread.join();
            }
        }
    }

    fn run_test_session_server(
        runtime_directory: &Path,
        session_id: SessionId,
        fake_pty_backend: Arc<FakePtyBackend>,
        runtime_event_receiver: mpsc::Receiver<RuntimeEvent>,
        runtime_event_sender: mpsc::Sender<RuntimeEvent>,
    ) {
        let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend;
        let mut session_server = Server::from_runtime_parts(
            pty_backend,
            runtime_event_receiver,
            runtime_event_sender.clone(),
        );
        session_server.load_startup_config(None);
        session_server
            .bootstrap_session(
                session_id,
                "quiet-lake".to_string(),
                TEST_VIEWPORT_SIZE,
                SystemTime::now(),
                None,
            )
            .expect("the session is seeded");

        let ipc_server =
            IpcServer::start(runtime_directory, session_id, runtime_event_sender, None)
                .expect("the control socket binds");
        session_server.attach_ipc_server(ipc_server);

        loop {
            let current_time = Instant::now();
            let runtime_event = match session_server.next_render_wakeup(current_time) {
                Some(render_wakeup_timeout) => match session_server
                    .inbox_rx()
                    .recv_timeout(render_wakeup_timeout)
                {
                    Ok(runtime_event) => Some(runtime_event),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                },
                None => match session_server.inbox_rx().recv() {
                    Ok(runtime_event) => Some(runtime_event),
                    Err(_) => break,
                },
            };
            let mut should_quit = false;
            if let Some(runtime_event) = runtime_event {
                should_quit |= session_server
                    .handle_runtime_event(runtime_event)
                    .is_break();
            }
            while let Ok(runtime_event) = session_server.inbox_rx().try_recv() {
                should_quit |= session_server
                    .handle_runtime_event(runtime_event)
                    .is_break();
            }
            session_server.resync_lagged();
            if session_server.poll_render(Instant::now()) {
                session_server.push_frames();
            }
            if should_quit
                || session_server.is_quit_requested()
                || !session_server.has_active_panes()
            {
                break;
            }
        }
        session_server.shutdown();
    }

    /// Serve one TLS connection on loopback with the real [`serve_remote_connection`],
    /// answering its admission questions the way the router does: the secret
    /// is admitted host-wide and every locate answers `session_endpoint_path`.
    ///
    /// Returns the address the client dials.
    fn start_test_remote_listener(session_endpoint_path: PathBuf) -> String {
        let tls_config = Arc::new(
            build_server_config(&build_test_certificate()).expect("the TLS config builds"),
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test listener");
        let remote_listen_address = listener
            .local_addr()
            .expect("read the bound address")
            .to_string();

        let (dispatcher_events_sender, dispatcher_events_receiver) = mpsc::channel::<RouterEvent>();
        std::thread::spawn(move || {
            while let Ok(router_event) = dispatcher_events_receiver.recv() {
                let RouterEvent::Admission(admission_request) = router_event else {
                    continue;
                };
                match admission_request {
                    AdmissionAsk::Admit {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(Some(Admitted {
                            scope: TokenScope::HostWide,
                            remote_connection_id: 7,
                        }));
                    }
                    AdmissionAsk::Rows {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(Vec::new());
                    }
                    AdmissionAsk::Locate {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(Some(session_endpoint_path.clone()));
                    }
                    AdmissionAsk::Ended { .. } => {}
                }
            }
        });

        std::thread::spawn(move || {
            let (accepted_socket, _) = listener.accept().expect("the listener accepts the client");
            let admission_slot = AdmissionSlot::enter_admission(&Arc::new(AtomicUsize::new(0)))
                .expect("a fresh count admits");
            serve_remote_connection(
                accepted_socket,
                &tls_config,
                &dispatcher_events_sender,
                admission_slot,
            );
        });

        remote_listen_address
    }

    /// Submit `command` to the session over its own control connection, the
    /// way `koshi <verb>` does from outside every pane.
    fn submit_session_command(running_session: &RunningSession, command: Command) -> CommandResult {
        let session_endpoint = EndpointFile::load_from_path(&running_session.get_endpoint_path())
            .expect("the session server advertises its socket");
        let mut connection = Connection::connect(&session_endpoint.socket_address)
            .expect("the control socket answers");
        connection
            .send(&IpcRequest {
                request_id: 1,
                request_kind: IpcRequestKind::Hello {
                    min_protocol_version: get_agreed_minimum_protocol_version(),
                    max_protocol_version: get_agreed_maximum_protocol_version(),
                    connection_token: session_endpoint.connection_token.clone(),
                    is_remote: false,
                },
            })
            .expect("the server reads the Hello");
        let hello_response: IpcResponse = connection.recv().expect("the server answers the Hello");
        match hello_response.answer_result {
            IpcResult::Hello {
                protocol_version, ..
            } => assert_eq!(protocol_version, get_agreed_maximum_protocol_version()),
            unexpected_response => panic!("the Hello was answered with {unexpected_response:?}"),
        }

        let envelope = CommandEnvelope::from_parts(
            CommandId::new(),
            CommandSource::from_external_cli(Some(running_session.session_id), None),
            SystemTime::now(),
            command,
        );
        connection
            .send(&IpcRequest {
                request_id: 2,
                request_kind: IpcRequestKind::SubmitCommand(Box::new(envelope)),
            })
            .expect("the server reads the command");
        let command_response: IpcResponse =
            connection.recv().expect("the server answers the command");
        match command_response.answer_result {
            IpcResult::CommandResult(command_result) => command_result,
            unexpected_response => panic!("the command was answered with {unexpected_response:?}"),
        }
    }

    fn get_agreed_minimum_protocol_version() -> u32 {
        koshi_ipc::protocol::MIN_PROTOCOL_VERSION
    }

    fn get_agreed_maximum_protocol_version() -> u32 {
        koshi_ipc::protocol::PROTOCOL_VERSION
    }

    /// A key the remote client's keymap does not bind crosses the TLS
    /// doorway, the router's bridge and the session's control socket with
    /// every field the terminal reported. The shifted key is one of those
    /// fields, and it alone decides the byte the pane reads: key `1` with
    /// Shift and shifted key `!` writes `!`, never `1`.
    #[test]
    fn a_bridged_keyboard_request_reaches_the_pane_with_the_field_that_decides_its_byte() {
        let running_session = RunningSession::start_running_session();
        let remote_listen_address = start_test_remote_listener(running_session.get_endpoint_path());

        let link = remote_client::connect_remote_server(
            &remote_listen_address,
            &ConnectionToken::from_secret("testSecret"),
            None,
            Duration::from_secs(5),
            None,
        )
        .expect("the listener admits the client");
        let (mut reader, mut writer) = remote_client::attach_remote_session(
            link,
            SessionSelector::SessionId(running_session.session_id),
        )
        .expect("the attach request is written");

        let hello_response: IpcResponse = reader.recv().expect("the session answers the Hello");
        match hello_response.answer_result {
            IpcResult::Hello {
                protocol_version, ..
            } => assert_eq!(protocol_version, get_agreed_maximum_protocol_version()),
            unexpected_response => panic!("the Hello was answered with {unexpected_response:?}"),
        }

        writer
            .send(&IpcRequest {
                request_id: 2,
                request_kind: IpcRequestKind::Attach {
                    viewport: TEST_VIEWPORT_SIZE,
                    event_filter: EventFilterSpec::All,
                    resume_client_id: None,
                    resume_token: None,
                    pane_area: None,
                    graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                    cell_size: None,
                },
            })
            .expect("the attach is written");
        let attach_response: IpcResponse = reader.recv().expect("the session answers the attach");
        assert!(
            matches!(attach_response.answer_result, IpcResult::Attached { .. }),
            "expected an attach reply, got {:?}",
            attach_response.answer_result
        );

        let shifted_digit = KeyInput {
            key: KeyIdentity::Key(Key::Char('1')),
            key_event_kind: KeyEventKind::Press,
            shifted_key: Some('!'),
            base_layout_key: Some('1'),
            associated_text: "!".to_string(),
            modifier_flags: KeyModifierFlags::SHIFT,
        };
        writer
            .send(&IpcRequest {
                request_id: 3,
                request_kind: IpcRequestKind::Keyboard {
                    key_input: shifted_digit.clone(),
                },
            })
            .expect("the keyboard request is written");

        assert_eq!(
            running_session.wait_for_settled_pane_write_bytes(),
            vec![b"!".to_vec()],
            "the shifted key crossed the bridge and decided the byte"
        );

        // The release of the same key crosses too, and legacy pane delivery
        // writes nothing for it.
        writer
            .send(&IpcRequest {
                request_id: 4,
                request_kind: IpcRequestKind::Keyboard {
                    key_input: KeyInput {
                        key_event_kind: KeyEventKind::Release,
                        ..shifted_digit
                    },
                },
            })
            .expect("the release is written");

        assert_eq!(
            running_session.wait_for_settled_pane_write_bytes(),
            vec![b"!".to_vec()],
            "the release added no byte"
        );
    }

    #[test]
    fn a_bridged_client_keeps_receiving_events_while_the_link_is_held_open() {
        let running_session = RunningSession::start_running_session();
        let remote_listen_address = start_test_remote_listener(running_session.get_endpoint_path());

        // Dial through the real TLS doorway and attach, the way
        // `koshi attach --remote` does.
        let link = remote_client::connect_remote_server(
            &remote_listen_address,
            &ConnectionToken::from_secret("testSecret"),
            None,
            Duration::from_secs(5),
            None,
        )
        .expect("the listener admits the client");
        let (mut reader, mut writer) = remote_client::attach_remote_session(
            link,
            SessionSelector::SessionId(running_session.session_id),
        )
        .expect("the attach request is written");

        // The session server's own Hello answer arrives through the bridge.
        let hello_response: IpcResponse = reader.recv().expect("the session answers the Hello");
        match hello_response.answer_result {
            IpcResult::Hello {
                protocol_version, ..
            } => assert_eq!(protocol_version, get_agreed_maximum_protocol_version()),
            unexpected_response => panic!("the Hello was answered with {unexpected_response:?}"),
        }

        writer
            .send(&IpcRequest {
                request_id: 2,
                request_kind: IpcRequestKind::Attach {
                    viewport: TEST_VIEWPORT_SIZE,
                    event_filter: EventFilterSpec::All,
                    resume_client_id: None,
                    resume_token: None,
                    pane_area: None,
                    graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities::default(),
                    cell_size: None,
                },
            })
            .expect("the attach is written");
        let attach_response: IpcResponse = reader.recv().expect("the session answers the attach");
        let IpcResult::Attached { client_id, .. } = attach_response.answer_result else {
            panic!(
                "expected an attach reply, got {:?}",
                attach_response.answer_result
            );
        };

        // A second tab, so each focus change below moves focus and emits a
        // critical event.
        let new_tab_command_result =
            submit_session_command(&running_session, Command::NewTab(NewTabArgs::default()));
        assert!(
            matches!(new_tab_command_result, CommandResult::Ok { .. }),
            "the second tab was refused: {new_tab_command_result:?}"
        );

        // Drive the session for the whole hold, one focus change at a time,
        // and require each one's event on the remote connection promptly.
        let hold_end_time = Instant::now() + LINK_HOLD_DURATION;
        let mut focus_round_count: u32 = 0;
        while Instant::now() < hold_end_time {
            let focus_command_result = submit_session_command(
                &running_session,
                Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Next,
                    client_id: Some(client_id),
                }),
            );
            let focused_tab_id = match &focus_command_result {
                CommandResult::Ok { emitted_events, .. } => match emitted_events.as_slice() {
                    [Event::TabFocused(tab_focused_event), ..] => tab_focused_event.tab_id,
                    unexpected_events => {
                        panic!("expected a focus event, got {unexpected_events:?}")
                    }
                },
                unexpected_command_result => {
                    panic!("the focus change was refused: {unexpected_command_result:?}")
                }
            };

            // Read frames until this round's focus event arrives. Painted
            // frames and other structure events on the way are read past.
            let session_event_deadline = Instant::now() + EVENT_DELIVERY_TIMEOUT_DURATION;
            loop {
                reader.set_deadline(Some(session_event_deadline));
                let session_event: SessionEvent = match reader.recv() {
                    Ok(session_event) => session_event,
                    Err(receive_error) => panic!(
                        "round {focus_round_count}: the remote stream gave no frame within \
                         {EVENT_DELIVERY_TIMEOUT_DURATION:?}: {receive_error}"
                    ),
                };
                match session_event {
                    SessionEvent::TabFocused { tab_id, .. } if tab_id == focused_tab_id => break,
                    _ => {}
                }
            }
            focus_round_count += 1;
            std::thread::sleep(Duration::from_millis(200));
        }

        assert!(
            focus_round_count >= 12,
            "the hold made only {focus_round_count} rounds; the link was not exercised"
        );
    }
}

/// A writer that records what it was given and the deadline it was handed.
struct DeadlineWriter {
    /// Every byte written, in order.
    written_frame_bytes: Vec<u8>,
    /// The deadline last set on this writer.
    deadline: Option<Instant>,
}

impl Write for DeadlineWriter {
    fn write(&mut self, source_bytes: &[u8]) -> io::Result<usize> {
        self.written_frame_bytes.extend_from_slice(source_bytes);
        Ok(source_bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Deadlined for DeadlineWriter {
    fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }
}

#[test]
fn a_refusal_written_before_admission_never_outlives_the_admission_window() {
    // The caller still holds its admission place while the refusal is written,
    // so that write cannot be given a window of its own past the deadline.
    let admission_deadline = Instant::now() + Duration::from_millis(1);
    let mut writer = DeadlineWriter {
        written_frame_bytes: Vec::new(),
        deadline: Some(admission_deadline),
    };

    send_refusal_with_deadline(&mut writer, compute_refusal_deadline(admission_deadline));

    assert_eq!(
        writer.deadline.expect("a refusal is given a deadline"),
        admission_deadline
    );
    assert_eq!(
        parse_remote_server_frames(&writer.written_frame_bytes),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        }],
        "the refusal frame is written"
    );
}

#[test]
fn a_refusal_after_admission_gets_the_whole_refusal_window() {
    let mut writer = DeadlineWriter {
        written_frame_bytes: Vec::new(),
        deadline: None,
    };
    let refusal_start_time = Instant::now();

    send_refusal(&mut writer);

    let assigned_deadline = writer.deadline.expect("a refusal is given a deadline");
    assert!(assigned_deadline >= refusal_start_time + REFUSAL_WINDOW_DURATION);
    assert!(assigned_deadline <= Instant::now() + REFUSAL_WINDOW_DURATION);
    assert_eq!(
        parse_remote_server_frames(&writer.written_frame_bytes),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
        }]
    );
}
