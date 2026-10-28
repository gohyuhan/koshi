//! Tests for the remote listener's connection-attempt table: how many
//! connections one address may have unadmitted inside a window, that an
//! admitted connection stops counting, that an address crossing its limit is
//! logged once rather than once per attempt, that a window ends on its own,
//! and that the table cannot grow past the count it is bounded at.
//!
//! Also [`RemoteConnectionEndReport`], which reports a bridged connection ended once, the two
//! functions that read and write one frame, the frames an admitted connection
//! sends, what [`bind_remote_listener`] refuses, the refusal of a caller
//! offering TLS 1.2 alone, the check of the router's program file that
//! each accepted connection runs, and the open of a session's bridge while
//! that session replaces its process image.
//!
//! [`serve_remote_connection`] is served over real TLS on loopback in two places: the
//! answers a caller reads before it is admitted, and one admitted client held
//! open while its session keeps emitting events.

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use super::*;

use koshi_core::ids::SessionId;
use koshi_ipc::protocol::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
use koshi_ipc::remote_state::CERTIFICATE_FILE_FORMAT;
use koshi_test_support::fixtures::{build_test_runtime_directory, write_printing_program};

/// The address `192.0.2.<last_ipv4_octet>`, for naming distinct callers in a test.
fn build_test_caller_ip_address(last_ipv4_octet: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, last_ipv4_octet))
}

/// A self-signed certificate naming `koshi`, generated fresh for one test
/// listener.
fn build_test_certificate() -> CertificateFile {
    let generated_certificate = rcgen::generate_simple_self_signed(vec!["koshi".to_string()])
        .expect("the test certificate generates");
    CertificateFile {
        file_format: CERTIFICATE_FILE_FORMAT,
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
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let current_time = Instant::now();

    for attempt_index in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
        assert_eq!(
            peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
            PeerAddressAttemptDecision::Serve {
                window_started_at: current_time,
            },
            "attempt {attempt_index} of {MAX_UNADMITTED_ATTEMPT_COUNT} is inside the limit"
        );
    }
}

#[test]
fn crossing_the_limit_is_logged_once_and_then_dropped_in_silence() {
    // One log line per address per window, not one per attempt.
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let current_time = Instant::now();

    for _ in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
        assert_eq!(
            peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
            PeerAddressAttemptDecision::Serve {
                window_started_at: current_time,
            }
        );
    }

    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
        PeerAddressAttemptDecision::DropAndSay,
        "the attempt that crosses the limit is the one that says so"
    );
    for refusal_index in 0..50 {
        assert_eq!(
            peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
            PeerAddressAttemptDecision::DropInSilence,
            "attempt {refusal_index} past the limit is dropped without a log line"
        );
    }
}

#[test]
fn a_window_that_has_passed_lets_the_same_address_back_in() {
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let window_started_at = Instant::now();

    for _ in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
        assert_eq!(
            peer_address_rate_table
                .decide_attempt(build_test_caller_ip_address(1), window_started_at),
            PeerAddressAttemptDecision::Serve { window_started_at }
        );
    }
    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), window_started_at),
        PeerAddressAttemptDecision::DropAndSay
    );

    // One second before the window ends the address is still shut out.
    let before_window_end =
        add_seconds_to_instant(window_started_at, RATE_WINDOW_DURATION.as_secs() - 1);
    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), before_window_end),
        PeerAddressAttemptDecision::DropInSilence
    );

    // Once the window has passed the address starts a fresh one.
    let after_window_end =
        add_seconds_to_instant(window_started_at, RATE_WINDOW_DURATION.as_secs() + 1);
    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), after_window_end),
        PeerAddressAttemptDecision::Serve {
            window_started_at: after_window_end,
        }
    );
}

#[test]
fn each_address_is_counted_on_its_own() {
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let current_time = Instant::now();

    for _ in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
        assert_eq!(
            peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
            PeerAddressAttemptDecision::Serve {
                window_started_at: current_time,
            }
        );
    }
    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
        PeerAddressAttemptDecision::DropAndSay
    );

    // A second address has opened nothing: its first attempt is served.
    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(2), current_time),
        PeerAddressAttemptDecision::Serve {
            window_started_at: current_time,
        }
    );
}

#[test]
fn peer_address_rate_table_never_exceeds_its_entry_limit() {
    // Every address gets one attempt: only the bound keeps the table down.
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let current_time = Instant::now();

    for address_index in 0..u32::try_from(MAX_RATE_TABLE_ENTRY_COUNT).expect("the bound fits") + 500
    {
        let peer_ip_address = IpAddr::V4(Ipv4Addr::from(address_index));
        peer_address_rate_table.decide_attempt(peer_ip_address, current_time);
        assert!(
            peer_address_rate_table.window_by_peer_ip_address.len() <= MAX_RATE_TABLE_ENTRY_COUNT,
            "the table holds {} addresses after {address_index} of them, past the {MAX_RATE_TABLE_ENTRY_COUNT} bound",
            peer_address_rate_table.window_by_peer_ip_address.len()
        );
    }
    assert_eq!(
        peer_address_rate_table.window_by_peer_ip_address.len(),
        MAX_RATE_TABLE_ENTRY_COUNT
    );
}

#[test]
fn a_full_table_drops_the_address_whose_window_opened_first() {
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let window_started_at = Instant::now();

    // Fill the table, each address one second after the one before it, so the
    // first one in is plainly the oldest.
    for address_index in 0..u32::try_from(MAX_RATE_TABLE_ENTRY_COUNT).expect("the bound fits") {
        let attempt_time = add_seconds_to_instant(
            window_started_at,
            u64::from(address_index) % (RATE_WINDOW_DURATION.as_secs() - 1),
        );
        peer_address_rate_table
            .decide_attempt(IpAddr::V4(Ipv4Addr::from(address_index)), attempt_time);
    }
    assert_eq!(
        peer_address_rate_table.window_by_peer_ip_address.len(),
        MAX_RATE_TABLE_ENTRY_COUNT
    );
    let oldest_peer_ip_address = peer_address_rate_table
        .window_by_peer_ip_address
        .iter()
        .min_by_key(|(_, peer_address_rate_window)| peer_address_rate_window.window_started_at)
        .map(|(peer_ip_address, _)| *peer_ip_address)
        .expect("a full table holds an oldest address");

    peer_address_rate_table.decide_attempt(build_test_caller_ip_address(200), window_started_at);

    assert!(
        !peer_address_rate_table
            .window_by_peer_ip_address
            .contains_key(&oldest_peer_ip_address),
        "the address whose window opened first left to make room"
    );
    assert!(
        peer_address_rate_table
            .window_by_peer_ip_address
            .contains_key(&build_test_caller_ip_address(200)),
        "the address that arrived took the room it made"
    );
    assert_eq!(
        peer_address_rate_table.window_by_peer_ip_address.len(),
        MAX_RATE_TABLE_ENTRY_COUNT
    );
}

/// One connection from `::1` counted in a fresh [`PeerAddressRateTable`].
fn build_test_counted_attempt() -> CountedAttempt {
    let peer_ip_address = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let PeerAddressAttemptDecision::Serve { window_started_at } =
        peer_address_rate_table.decide_attempt(peer_ip_address, Instant::now())
    else {
        panic!("a fresh table serves the first attempt");
    };
    CountedAttempt {
        peer_address_rate_table: Arc::new(Mutex::new(peer_address_rate_table)),
        peer_ip_address,
        window_started_at,
    }
}

#[test]
fn an_admitted_attempt_stops_counting_and_lets_one_more_from_its_address_in() {
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let current_time = Instant::now();
    for _ in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
        assert_eq!(
            peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
            PeerAddressAttemptDecision::Serve {
                window_started_at: current_time,
            }
        );
    }

    peer_address_rate_table.remove_attempt(build_test_caller_ip_address(1), current_time);

    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
        PeerAddressAttemptDecision::Serve {
            window_started_at: current_time,
        }
    );
    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
        PeerAddressAttemptDecision::DropAndSay
    );
}

#[test]
fn a_dropped_attempt_is_not_counted_and_the_limit_is_logged_once_per_window() {
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let current_time = Instant::now();
    for _ in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time);
    }
    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
        PeerAddressAttemptDecision::DropAndSay
    );
    for _ in 0..5 {
        assert_eq!(
            peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
            PeerAddressAttemptDecision::DropInSilence
        );
    }

    for _ in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
        peer_address_rate_table.remove_attempt(build_test_caller_ip_address(1), current_time);
    }

    for attempt_index in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
        assert_eq!(
            peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
            PeerAddressAttemptDecision::Serve {
                window_started_at: current_time,
            },
            "attempt {attempt_index} is served: the six dropped ones were never counted"
        );
    }
    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), current_time),
        PeerAddressAttemptDecision::DropInSilence,
        "this window already logged its limit"
    );
}

#[test]
fn an_attempt_counted_in_a_window_that_has_passed_is_not_taken_out_of_the_next_one() {
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let first_window_started_at = Instant::now();
    peer_address_rate_table
        .decide_attempt(build_test_caller_ip_address(1), first_window_started_at);
    let next_window_started_at = first_window_started_at + RATE_WINDOW_DURATION;
    for _ in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
        assert_eq!(
            peer_address_rate_table
                .decide_attempt(build_test_caller_ip_address(1), next_window_started_at),
            PeerAddressAttemptDecision::Serve {
                window_started_at: next_window_started_at,
            }
        );
    }

    peer_address_rate_table
        .remove_attempt(build_test_caller_ip_address(1), first_window_started_at);

    assert_eq!(
        peer_address_rate_table
            .decide_attempt(build_test_caller_ip_address(1), next_window_started_at),
        PeerAddressAttemptDecision::DropAndSay
    );
}

#[test]
fn removing_an_attempt_of_an_address_the_table_does_not_hold_changes_nothing() {
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let current_time = Instant::now();

    peer_address_rate_table.remove_attempt(build_test_caller_ip_address(1), current_time);

    assert_eq!(peer_address_rate_table.window_by_peer_ip_address.len(), 0);
}

#[test]
fn a_bridged_connection_is_reported_ended_once_however_many_directions_get_there() {
    // Both directions end the connection and either may end first.
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let remote_connection_end_report =
        RemoteConnectionEndReport::from_sender_and_connection_id(router_events_sender, 7);

    remote_connection_end_report.report_once();
    remote_connection_end_report.report_once();
    remote_connection_end_report.report_once();

    let end_report_event = router_events_receiver
        .try_recv()
        .expect("the first report arrives");
    let RouterEvent::Admission(AdmissionAsk::RemoveConnection {
        remote_connection_id,
    }) = end_report_event
    else {
        panic!("a bridged connection ending sends RemoveConnection");
    };
    assert_eq!(remote_connection_id, 7);
    assert_eq!(
        router_events_receiver.try_recv().err(),
        Some(mpsc::TryRecvError::Empty),
        "and nothing follows it, however many directions reported"
    );
}

#[test]
fn a_connection_reported_ended_by_the_direction_that_finished_first_needs_no_second() {
    // One direction reporting is enough; the other may still be blocked.
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let remote_connection_end_report = Arc::new(
        RemoteConnectionEndReport::from_sender_and_connection_id(router_events_sender, 3),
    );

    let inbound_end_report = Arc::clone(&remote_connection_end_report);
    std::thread::spawn(move || inbound_end_report.report_once())
        .join()
        .expect("the direction that finished first reports");

    let end_report_event = router_events_receiver.recv().expect("the report arrives");
    let RouterEvent::Admission(AdmissionAsk::RemoveConnection {
        remote_connection_id,
    }) = end_report_event
    else {
        panic!("a bridged connection ending sends RemoveConnection");
    };
    assert_eq!(
        remote_connection_id, 3,
        "without waiting for the other direction"
    );
}

#[test]
fn concurrent_end_reports_send_one_admission() {
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let remote_connection_end_report = Arc::new(
        RemoteConnectionEndReport::from_sender_and_connection_id(router_events_sender, 8),
    );
    let mut report_threads = Vec::new();

    for _ in 0..8 {
        let remote_connection_end_report = Arc::clone(&remote_connection_end_report);
        report_threads.push(std::thread::spawn(move || {
            remote_connection_end_report.report_once()
        }));
    }
    for report_thread in report_threads {
        report_thread.join().expect("the report thread ends");
    }

    let end_report_events: Vec<_> = router_events_receiver.try_iter().collect();
    assert_eq!(
        end_report_events.len(),
        1,
        "concurrent directions report the end once"
    );
    let RouterEvent::Admission(AdmissionAsk::RemoveConnection {
        remote_connection_id,
    }) = &end_report_events[0]
    else {
        panic!("the report is a RemoveConnection admission");
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
    // The window ends at RATE_WINDOW_DURATION, not one moment after it: an
    // attempt arriving at exactly that reading opens a new window and is served.
    let mut peer_address_rate_table = PeerAddressRateTable::new();
    let window_started_at = Instant::now();
    for _ in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
        assert_eq!(
            peer_address_rate_table
                .decide_attempt(build_test_caller_ip_address(1), window_started_at),
            PeerAddressAttemptDecision::Serve { window_started_at }
        );
    }
    assert_eq!(
        peer_address_rate_table.decide_attempt(build_test_caller_ip_address(1), window_started_at),
        PeerAddressAttemptDecision::DropAndSay
    );

    assert_eq!(
        peer_address_rate_table.decide_attempt(
            build_test_caller_ip_address(1),
            window_started_at + RATE_WINDOW_DURATION - Duration::from_millis(1),
        ),
        PeerAddressAttemptDecision::DropInSilence,
        "one millisecond before the window ends the address is still shut out"
    );
    assert_eq!(
        peer_address_rate_table.decide_attempt(
            build_test_caller_ip_address(1),
            window_started_at + RATE_WINDOW_DURATION,
        ),
        PeerAddressAttemptDecision::Serve {
            window_started_at: window_started_at + RATE_WINDOW_DURATION,
        },
        "at exactly {RATE_WINDOW_DURATION:?} the window is over"
    );
}

/// One frame's bytes as a caller sends them: a 4-byte big-endian length, then
/// the JSON.
fn build_remote_client_frame_bytes(remote_client_frame: &impl serde::Serialize) -> Vec<u8> {
    let payload_bytes = serde_json::to_vec(remote_client_frame).expect("the frame encodes");
    let frame_byte_count =
        u32::try_from(payload_bytes.len()).expect("a test frame fits in a length prefix");
    let mut frame_bytes = frame_byte_count.to_be_bytes().to_vec();
    frame_bytes.extend_from_slice(&payload_bytes);
    frame_bytes
}

#[test]
fn remote_client_frame_at_the_cap_is_read_and_one_byte_over_is_closed() {
    // A frame whose payload length equals the cap is read. One byte over the
    // cap closes the connection before the payload is read.
    let sent_remote_client_frame = RemoteClientFrame::Attach {
        session_selector: SessionSelector::SessionName("S-quiet-lake".to_string()),
        link_timeout_ms: Some(30_000),
    };
    let remote_client_frame_bytes = build_remote_client_frame_bytes(&sent_remote_client_frame);
    let payload_byte_count =
        u32::try_from(remote_client_frame_bytes.len() - 4).expect("the payload fits");

    let mut exact_limit_frame_stream = Cursor::new(remote_client_frame_bytes.clone());
    assert_eq!(
        read_client_frame(&mut exact_limit_frame_stream, payload_byte_count),
        RemoteClientFrameRead::Frame(sent_remote_client_frame)
    );
    assert_eq!(
        exact_limit_frame_stream.position(),
        u64::try_from(remote_client_frame_bytes.len()).expect("the frame fits"),
        "and the whole frame was taken off the stream"
    );

    let mut over_cap_frame_stream = Cursor::new(remote_client_frame_bytes);
    assert_eq!(
        read_client_frame(&mut over_cap_frame_stream, payload_byte_count - 1),
        RemoteClientFrameRead::Closed,
        "a length one byte over the cap closes the connection"
    );
    assert_eq!(
        over_cap_frame_stream.position(),
        4,
        "and its payload is never read"
    );
}

#[test]
fn unreadable_remote_client_json_is_refused_and_truncated_frame_is_closed() {
    // Unreadable bytes read as `Unreadable`, and the caller gets a refusal. A
    // stream that ends early reads as `Closed`, and nothing is written back.
    let unrecognized_remote_client_json = br#"{"Nonsense":1}"#.to_vec();
    let mut unreadable_json_frame_bytes = u32::try_from(unrecognized_remote_client_json.len())
        .expect("the unrecognized JSON fits")
        .to_be_bytes()
        .to_vec();
    unreadable_json_frame_bytes.extend_from_slice(&unrecognized_remote_client_json);
    assert_eq!(
        read_client_frame(
            &mut Cursor::new(unreadable_json_frame_bytes),
            REMOTE_HELLO_MAX_BYTE_COUNT
        ),
        RemoteClientFrameRead::Unreadable,
        "a whole frame carrying JSON this build has no frame for is refused"
    );

    // The length says ten bytes and three follow.
    let mut truncated_remote_client_frame_bytes = 10u32.to_be_bytes().to_vec();
    truncated_remote_client_frame_bytes.extend_from_slice(b"abc");
    assert_eq!(
        read_client_frame(
            &mut Cursor::new(truncated_remote_client_frame_bytes),
            REMOTE_HELLO_MAX_BYTE_COUNT,
        ),
        RemoteClientFrameRead::Closed,
        "a payload that ends early closes the connection"
    );

    assert_eq!(
        read_client_frame(
            &mut Cursor::new(vec![0u8, 0, 1]),
            REMOTE_HELLO_MAX_BYTE_COUNT
        ),
        RemoteClientFrameRead::Closed,
        "and so does a length prefix that ends early"
    );
}

/// The Hello koshi 0.3.0 and 0.4.0 open with: remote protocol version 1 only,
/// under the field names those releases send.
fn build_previous_release_remote_hello_json() -> serde_json::Value {
    serde_json::json!({
        "Hello": {
            "min_remote_version": 1,
            "max_remote_version": 1,
            "min_protocol_version": 2,
            "max_protocol_version": 3,
            "token": "k7QxSecret",
        }
    })
}

#[test]
fn the_hello_of_koshi_0_4_0_reads_as_the_remote_protocol_range_it_names() {
    let previous_release_hello_frame_bytes =
        build_remote_client_frame_bytes(&build_previous_release_remote_hello_json());

    assert_eq!(
        read_client_frame(
            &mut Cursor::new(previous_release_hello_frame_bytes),
            REMOTE_HELLO_MAX_BYTE_COUNT
        ),
        RemoteClientFrameRead::PreviousReleaseHello {
            minimum_remote_version: 1,
            maximum_remote_version: 1,
        }
    );
}

#[test]
fn empty_remote_client_payload_is_unreadable() {
    // A length of zero is inside every cap. The four length bytes come off the
    // stream and the empty payload fails to decode.
    let mut empty_payload_frame_stream = Cursor::new(0u32.to_be_bytes().to_vec());

    assert_eq!(
        read_client_frame(&mut empty_payload_frame_stream, REMOTE_HELLO_MAX_BYTE_COUNT),
        RemoteClientFrameRead::Unreadable,
        "a frame naming no payload is refused rather than read"
    );
    assert_eq!(
        empty_payload_frame_stream.position(),
        4,
        "and its four length bytes were taken"
    );
}

#[test]
fn remote_server_frame_uses_a_big_endian_length_prefix() {
    // The length prefix is big-endian: its four bytes differ from the
    // little-endian bytes of the same length.
    let remote_server_frame = RemoteServerFrame::Refused {
        message: REMOTE_REFUSED.to_string(),
        remote_protocol_version: None,
    };
    let payload_bytes = serde_json::to_vec(&remote_server_frame).expect("the frame encodes");
    let frame_byte_count = u32::try_from(payload_bytes.len()).expect("the answer fits");
    let mut expected_frame_bytes = frame_byte_count.to_be_bytes().to_vec();
    expected_frame_bytes.extend_from_slice(&payload_bytes);

    let mut written_frame_bytes = Vec::new();
    send_remote_frame(&mut written_frame_bytes, &remote_server_frame)
        .expect("the answer is written");

    assert_eq!(written_frame_bytes, expected_frame_bytes);
    assert_ne!(
        written_frame_bytes[..4],
        frame_byte_count.to_le_bytes(),
        "a {} byte payload names different bytes each way round",
        payload_bytes.len()
    );
}

/// A `Vec`-backed writing half standing in for the TLS one: bytes are
/// recorded, and a deadline and an idle timeout are taken and ignored.
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
    fn set_deadline(&mut self, _write_deadline: Option<Instant>) {}

    fn set_idle_timeout(&mut self, _idle_timeout: Option<Duration>) {}
}

/// A writing half that takes one byte per call, standing in for a socket that
/// accepts a little at a time.
struct OneByteWriter {
    written_frame_bytes: Vec<u8>,
}

impl Write for OneByteWriter {
    fn write(&mut self, source_bytes: &[u8]) -> io::Result<usize> {
        match source_bytes.first() {
            Some(first_byte) => {
                self.written_frame_bytes.push(*first_byte);
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
fn send_remote_frame_writes_every_byte_to_a_one_byte_writer() {
    // Every byte of the frame goes out, however little the socket takes per
    // call.
    let remote_server_frame = RemoteServerFrame::Welcome {
        remote_protocol_version: REMOTE_PROTOCOL_VERSION,
    };
    let payload_bytes = serde_json::to_vec(&remote_server_frame).expect("the frame encodes");
    let frame_byte_count = u32::try_from(payload_bytes.len()).expect("the answer fits");
    let mut expected_frame_bytes = frame_byte_count.to_be_bytes().to_vec();
    expected_frame_bytes.extend_from_slice(&payload_bytes);

    let mut one_byte_writer = OneByteWriter {
        written_frame_bytes: Vec::new(),
    };
    send_remote_frame(&mut one_byte_writer, &remote_server_frame).expect("the answer is written");

    assert_eq!(one_byte_writer.written_frame_bytes, expected_frame_bytes);
}

#[test]
fn send_remote_frame_returns_a_broken_pipe_error() {
    let remote_server_frame = RemoteServerFrame::Refused {
        message: REMOTE_REFUSED.to_string(),
        remote_protocol_version: None,
    };

    let frame_write_error = send_remote_frame(&mut BrokenWriter, &remote_server_frame)
        .expect_err("a refused write is reported");

    assert_eq!(frame_write_error.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(frame_write_error.to_string(), "the caller hung up");
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
    // The frame loop answers a `List` and reads the next frame: one connection
    // lists the sessions and then attaches to one.
    let session_id = SessionId::new();
    let located_session = LocatedSession {
        runtime_directory: PathBuf::from("/home/user/.koshi"),
        session_id,
    };
    let held_located_session = located_session.clone();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || loop {
        match router_events_receiver.recv() {
            Ok(RouterEvent::Admission(AdmissionAsk::ListRows {
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
                let _ = response_sender.send(Ok(held_located_session.clone()));
            }
            _ => return,
        }
    });

    let mut request_frame_bytes = build_remote_client_frame_bytes(&RemoteClientFrame::List);
    request_frame_bytes.extend(build_remote_client_frame_bytes(
        &RemoteClientFrame::Attach {
            session_selector: SessionSelector::SessionId(session_id),
            link_timeout_ms: Some(30_000),
        },
    ));
    let mut reader = Cursor::new(request_frame_bytes);
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = RemoteConnectionAdmission {
        scope: TokenScope::HostWide,
        remote_connection_id: 7,
    };

    let attached_session = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &router_events_sender,
    );

    assert_eq!(attached_session, Some((located_session, Some(30_000))));
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
    drop(router_events_sender);
    dispatcher_thread
        .join()
        .expect("the stand-in dispatcher ended");
}

#[test]
fn a_second_hello_on_an_admitted_connection_is_refused() {
    // The Hello belongs to admission; an admitted connection sending another
    // one is refused and the connection ends unattached.
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut reader = Cursor::new(build_remote_client_frame_bytes(&RemoteClientFrame::Hello {
        minimum_remote_version: 1,
        maximum_remote_version: 1,
        minimum_protocol_version: 1,
        maximum_protocol_version: 1,
        connection_token: ConnectionToken::from_secret("alreadyAdmitted"),
    }));
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = RemoteConnectionAdmission {
        scope: TokenScope::HostWide,
        remote_connection_id: 3,
    };

    let attached_session = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &router_events_sender,
    );

    assert_eq!(attached_session, None);
    assert_eq!(
        parse_remote_server_frames(&writer.written_frame_bytes),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
            remote_protocol_version: None,
        }],
    );
}

#[test]
fn the_hello_of_koshi_0_4_0_on_an_admitted_connection_is_refused() {
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut reader = Cursor::new(build_remote_client_frame_bytes(
        &build_previous_release_remote_hello_json(),
    ));
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = RemoteConnectionAdmission {
        scope: TokenScope::HostWide,
        remote_connection_id: 3,
    };

    let attached_session = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &router_events_sender,
    );

    assert_eq!(attached_session, None);
    assert_eq!(
        parse_remote_server_frames(&writer.written_frame_bytes),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
            remote_protocol_version: None,
        }],
    );
}

/// A located session under `/home/user/.koshi`, with a fresh session id.
fn build_located_session() -> LocatedSession {
    LocatedSession {
        runtime_directory: PathBuf::from("/home/user/.koshi"),
        session_id: SessionId::new(),
    }
}

/// What [`process_admitted_remote_frames`] did with one admitted Attach.
struct ServedAttach {
    /// What the frame loop returned.
    attached_session: Option<(LocatedSession, Option<u32>)>,
    /// Every frame the frame loop wrote.
    written_server_frames: Vec<RemoteServerFrame>,
    /// How many times the frame loop asked the dispatcher where the session
    /// listens.
    locate_ask_count: usize,
}

/// Run one admitted Attach naming `link_timeout_ms` for a fresh session id
/// against a stand-in dispatcher that answers its Locate with
/// `locate_answer`, and hand back what the frame loop did.
fn serve_attach_against_locate_answer(
    locate_answer: Result<LocatedSession, LocateRefusal>,
    link_timeout_ms: Option<u32>,
) -> ServedAttach {
    let session_id = SessionId::new();
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    let dispatcher_thread = std::thread::spawn(move || {
        let mut locate_answer = Some(locate_answer);
        let mut locate_ask_count = 0;
        for router_event in router_events_receiver {
            let RouterEvent::Admission(AdmissionAsk::Locate {
                session_selector,
                response_sender,
                ..
            }) = router_event
            else {
                panic!("an attach asks the dispatcher only where the session listens");
            };
            assert_eq!(session_selector, SessionSelector::SessionId(session_id));
            locate_ask_count += 1;
            let _ = response_sender.send(locate_answer.take().expect("one Locate per attach"));
        }
        locate_ask_count
    });

    let mut reader = Cursor::new(build_remote_client_frame_bytes(
        &RemoteClientFrame::Attach {
            session_selector: SessionSelector::SessionId(session_id),
            link_timeout_ms,
        },
    ));
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = RemoteConnectionAdmission {
        scope: TokenScope::HostWide,
        remote_connection_id: 11,
    };

    let attached_session = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &router_events_sender,
    );
    drop(router_events_sender);
    let locate_ask_count = dispatcher_thread
        .join()
        .expect("the stand-in dispatcher ended");
    ServedAttach {
        attached_session,
        written_server_frames: parse_remote_server_frames(&writer.written_frame_bytes),
        locate_ask_count,
    }
}

#[test]
fn an_attach_the_dispatcher_refuses_ends_the_connection_unattached() {
    // A session that does not exist and a session the scope does not cover
    // both reach this loop as the same `LocateRefusal::NotReached`.
    let served_attach =
        serve_attach_against_locate_answer(Err(LocateRefusal::NotReached), Some(30_000));

    assert_eq!(served_attach.attached_session, None);
    assert_eq!(
        served_attach.written_server_frames,
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
            remote_protocol_version: None,
        }],
    );
    assert_eq!(served_attach.locate_ask_count, 1);
}

#[test]
fn an_attach_arriving_while_the_router_restarts_is_refused_with_the_restarting_sentence() {
    let served_attach =
        serve_attach_against_locate_answer(Err(LocateRefusal::RouterRestarting), Some(30_000));

    assert_eq!(served_attach.attached_session, None);
    assert_eq!(
        served_attach.written_server_frames,
        vec![RemoteServerFrame::Refused {
            message: ROUTER_RESTARTING_MESSAGE.to_string(),
            remote_protocol_version: None,
        }],
    );
    assert_eq!(served_attach.locate_ask_count, 1);
}

#[test]
fn an_attach_the_dispatcher_locates_hands_back_the_located_session_and_its_link_timeout() {
    let located_session = build_located_session();

    let served_attach =
        serve_attach_against_locate_answer(Ok(located_session.clone()), Some(MIN_LINK_TIMEOUT_MS));

    assert_eq!(
        served_attach.attached_session,
        Some((located_session, Some(MIN_LINK_TIMEOUT_MS)))
    );
    assert_eq!(served_attach.written_server_frames, Vec::new());
    assert_eq!(served_attach.locate_ask_count, 1);
}

#[test]
fn an_attach_naming_a_link_timeout_below_the_minimum_is_refused_before_the_dispatcher_is_asked() {
    let served_attach = serve_attach_against_locate_answer(
        Ok(build_located_session()),
        Some(MIN_LINK_TIMEOUT_MS - 1),
    );

    assert_eq!(served_attach.attached_session, None);
    assert_eq!(
        served_attach.written_server_frames,
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
            remote_protocol_version: None,
        }],
    );
    assert_eq!(
        served_attach.locate_ask_count, 0,
        "the dispatcher is never asked"
    );
}

#[test]
fn an_attach_naming_no_link_timeout_hands_back_none_for_it() {
    let located_session = build_located_session();

    let served_attach = serve_attach_against_locate_answer(Ok(located_session.clone()), None);

    assert_eq!(
        served_attach.attached_session,
        Some((located_session, None))
    );
    assert_eq!(served_attach.written_server_frames, Vec::new());
    assert_eq!(served_attach.locate_ask_count, 1);
}

#[test]
fn bytes_an_admitted_connection_sends_that_are_not_a_frame_are_refused() {
    // After admission, JSON this build has no frame for reads as a refusal.
    let unrecognized_remote_client_json = br#"{"Nonsense":1}"#.to_vec();
    let mut request_frame_bytes = u32::try_from(unrecognized_remote_client_json.len())
        .expect("the unrecognized JSON fits")
        .to_be_bytes()
        .to_vec();
    request_frame_bytes.extend_from_slice(&unrecognized_remote_client_json);
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut reader = Cursor::new(request_frame_bytes);
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = RemoteConnectionAdmission {
        scope: TokenScope::HostWide,
        remote_connection_id: 5,
    };

    let attached_session = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &router_events_sender,
    );

    assert_eq!(attached_session, None);
    assert_eq!(
        parse_remote_server_frames(&writer.written_frame_bytes),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
            remote_protocol_version: None,
        }],
    );
}

#[test]
fn an_admitted_connection_that_hangs_up_is_answered_with_nothing() {
    let (router_events_sender, _router_events_receiver) = mpsc::channel();
    let mut reader = Cursor::new(Vec::new());
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = RemoteConnectionAdmission {
        scope: TokenScope::HostWide,
        remote_connection_id: 9,
    };

    let attached_session = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &router_events_sender,
    );

    assert_eq!(attached_session, None);
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
    let (router_events_sender, router_events_receiver) = mpsc::channel::<RouterEvent>();
    drop(router_events_receiver);
    let mut reader = Cursor::new(build_remote_client_frame_bytes(&RemoteClientFrame::List));
    let mut writer = RecordedWriter {
        written_frame_bytes: Vec::new(),
    };
    let admitted_connection = RemoteConnectionAdmission {
        scope: TokenScope::HostWide,
        remote_connection_id: 4,
    };

    let attached_session = process_admitted_remote_frames(
        &mut reader,
        &mut writer,
        &admitted_connection,
        &router_events_sender,
    );

    assert_eq!(attached_session, None);
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
                minimum_protocol_version: 1,
                maximum_protocol_version: 4,
                connection_token: ConnectionToken::from_secret("endpointSecret"),
                is_remote: true,
            },
        }
    );
}

#[test]
fn a_certificate_no_tls_configuration_accepts_takes_no_port() {
    let certificate_file = CertificateFile {
        file_format: CERTIFICATE_FILE_FORMAT,
        cert_der: vec![1, 2, 3],
        key_der: vec![4, 5, 6],
    };

    let Err(bind_error) =
        bind_remote_listener(SocketAddr::from(([127, 0, 0, 1], 0)), &certificate_file)
    else {
        panic!("bytes that are not a certificate build no TLS configuration");
    };

    assert_eq!(bind_error.kind(), io::ErrorKind::Other);
}

/// The bytes of one TLS record carrying a ClientHello that offers TLS 1.2
/// alone: its `supported_versions` extension lists `0x0303` only, beside one
/// cipher suite, null compression and one signature algorithm.
fn build_tls_1_2_only_client_hello_record() -> Vec<u8> {
    let mut client_hello_bytes = Vec::new();
    client_hello_bytes.extend_from_slice(&[0x03, 0x03]); // legacy_version: TLS 1.2
    client_hello_bytes.extend_from_slice(&[0u8; 32]); // random
    client_hello_bytes.push(0); // legacy_session_id: empty
    client_hello_bytes.extend_from_slice(&[0x00, 0x02, 0xc0, 0x2b]); // one cipher suite
    client_hello_bytes.extend_from_slice(&[0x01, 0x00]); // compression: null only
    let mut extension_bytes = Vec::new();
    // signature_algorithms: ecdsa_secp256r1_sha256 alone
    extension_bytes.extend_from_slice(&[0x00, 0x0d, 0x00, 0x04, 0x00, 0x02, 0x04, 0x03]);
    // supported_versions: TLS 1.2 alone
    extension_bytes.extend_from_slice(&[0x00, 0x2b, 0x00, 0x03, 0x02, 0x03, 0x03]);
    let extension_byte_count = u16::try_from(extension_bytes.len()).expect("a short list");
    client_hello_bytes.extend_from_slice(&extension_byte_count.to_be_bytes());
    client_hello_bytes.extend_from_slice(&extension_bytes);

    let client_hello_byte_count = u32::try_from(client_hello_bytes.len()).expect("a short hello");
    let mut handshake_bytes = vec![0x01]; // handshake type: client_hello
    handshake_bytes.extend_from_slice(&client_hello_byte_count.to_be_bytes()[1..]);
    handshake_bytes.extend_from_slice(&client_hello_bytes);

    let handshake_byte_count = u16::try_from(handshake_bytes.len()).expect("a short record");
    let mut record_bytes = vec![0x16, 0x03, 0x01]; // record type: handshake
    record_bytes.extend_from_slice(&handshake_byte_count.to_be_bytes());
    record_bytes.extend_from_slice(&handshake_bytes);
    record_bytes
}

#[test]
fn a_caller_offering_tls_1_2_alone_is_refused_by_the_server_configuration() {
    let tls_config = Arc::new(
        build_remote_server_tls_config(&build_test_certificate()).expect("the TLS config builds"),
    );
    let mut server_connection =
        ServerConnection::new(tls_config).expect("a server connection starts");

    server_connection
        .read_tls(&mut Cursor::new(build_tls_1_2_only_client_hello_record()))
        .expect("the record is read");
    let handshake_error = server_connection
        .process_new_packets()
        .expect_err("a TLS 1.2 caller is refused");

    assert_eq!(
        handshake_error,
        rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOfferedOrEnabled)
    );
}

#[test]
fn a_remote_connection_makes_the_restart_due_when_the_program_file_holds_another_version() {
    // The accept loop serves on a loopback port and is left running on its
    // thread when the test ends.
    let program_directory = build_test_runtime_directory();
    let program_path = write_printing_program(
        program_directory.path(),
        "koshi",
        &program_directory.path().join("started_runs"),
        "koshi 1.0.0",
    );
    let executable_watch = Arc::new(ExecutableWatch::from_executable_path(
        program_path.clone(),
        "1.0.0",
    ));
    let replacement_path = write_printing_program(
        program_directory.path(),
        "replacement",
        &program_directory.path().join("runs"),
        "koshi 9.9.9",
    );
    std::fs::rename(&replacement_path, &program_path).expect("the program file is replaced");
    let tls_config = Arc::new(
        build_remote_server_tls_config(&build_test_certificate()).expect("the TLS config builds"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test listener");
    let remote_listen_address = listener.local_addr().expect("read the bound address");
    let (router_events_sender, router_events_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        run_remote_accept_loop(
            &listener,
            &tls_config,
            &router_events_sender,
            &executable_watch,
        );
    });

    let _caller_stream =
        TcpStream::connect(remote_listen_address).expect("the caller reaches the remote port");

    let router_event = router_events_receiver.recv_timeout(Duration::from_secs(10));
    let Ok(RouterEvent::RestartDue) = router_event else {
        panic!("the dispatcher was not told the restart is due");
    };
}

mod admission_answers {
    //! What [`serve_remote_connection`] answers a caller before it is admitted,
    //! read by a real client over real TLS on loopback: a caller speaking no
    //! remote protocol version this build speaks, a caller opening with the
    //! Hello of koshi 0.3.0 or 0.4.0, a secret the dispatcher refuses, an
    //! opening frame that is not a Hello, and a caller the dispatcher admits.

    use super::*;

    use koshi_ipc::error::IpcError;
    use koshi_ipc::remote_wire;

    /// How long the client gives the whole dial: the connect, the TLS
    /// handshake, the opening frame and the one frame answering it.
    const REMOTE_DIAL_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

    /// Start a stand-in dispatcher and hand back the sender its events go to.
    ///
    /// `should_admit_connection` says what it answers an
    /// [`AdmissionAsk::Admit`] with: a host-wide scope numbered 7 when it is
    /// true, and a refusal when it is false. Every listing is empty and every
    /// locate is refused.
    fn spawn_test_admission_dispatcher(should_admit_connection: bool) -> Sender<RouterEvent> {
        let (dispatcher_events_sender, dispatcher_events_receiver) = mpsc::channel::<RouterEvent>();
        std::thread::spawn(move || {
            while let Ok(RouterEvent::Admission(admission_ask)) = dispatcher_events_receiver.recv()
            {
                match admission_ask {
                    AdmissionAsk::Admit {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(should_admit_connection.then_some(
                            RemoteConnectionAdmission {
                                scope: TokenScope::HostWide,
                                remote_connection_id: 7,
                            },
                        ));
                    }
                    AdmissionAsk::ListRows {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(Vec::new());
                    }
                    AdmissionAsk::Locate {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(Err(LocateRefusal::NotReached));
                    }
                    AdmissionAsk::RemoveConnection { .. } => {}
                }
            }
        });
        dispatcher_events_sender
    }

    /// A loopback listener presenting a fresh test certificate, the TLS
    /// configuration it serves with, and the fingerprint of that certificate.
    fn bind_test_tls_listener() -> (TcpListener, Arc<ServerConfig>, String) {
        let certificate_file = build_test_certificate();
        let certificate_fingerprint =
            tls::compute_certificate_fingerprint(&certificate_file.cert_der);
        let tls_config = Arc::new(
            build_remote_server_tls_config(&certificate_file).expect("the TLS config builds"),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test listener");
        (listener, tls_config, certificate_fingerprint)
    }

    /// Serve exactly one TLS connection on loopback with the real
    /// [`serve_remote_connection`] and the stand-in dispatcher
    /// [`spawn_test_admission_dispatcher`] starts for `should_admit_connection`.
    ///
    /// Returns the address the client dials and the fingerprint of the
    /// certificate the listener presents.
    fn start_test_remote_listener(should_admit_connection: bool) -> TestRemoteListener {
        let (listener, tls_config, certificate_fingerprint) = bind_test_tls_listener();
        let remote_listen_address = listener
            .local_addr()
            .expect("read the bound address")
            .to_string();
        let dispatcher_events_sender = spawn_test_admission_dispatcher(should_admit_connection);

        std::thread::spawn(move || {
            let (accepted_socket, _) = listener.accept().expect("the listener accepts the client");
            let admission_slot = AdmissionSlot::enter_admission(&Arc::new(AtomicUsize::new(0)))
                .expect("a fresh count admits");
            serve_remote_connection(
                accepted_socket,
                &tls_config,
                &dispatcher_events_sender,
                admission_slot,
                build_test_counted_attempt(),
            );
        });

        TestRemoteListener {
            remote_listen_address,
            certificate_fingerprint,
            _program_directory: None,
        }
    }

    /// Serve every TLS connection on loopback with the real
    /// [`run_remote_accept_loop`] and the stand-in dispatcher
    /// [`spawn_test_admission_dispatcher`] starts for `should_admit_connection`.
    /// The program file the accept loop watches does not exist.
    fn start_test_remote_accept_loop(should_admit_connection: bool) -> TestRemoteListener {
        let (listener, tls_config, certificate_fingerprint) = bind_test_tls_listener();
        let remote_listen_address = listener
            .local_addr()
            .expect("read the bound address")
            .to_string();
        let dispatcher_events_sender = spawn_test_admission_dispatcher(should_admit_connection);
        let program_directory = build_test_runtime_directory();
        let executable_watch = Arc::new(ExecutableWatch::from_executable_path(
            program_directory.path().join("koshi-that-is-not-there"),
            "1.0.0",
        ));

        std::thread::spawn(move || {
            run_remote_accept_loop(
                &listener,
                &tls_config,
                &dispatcher_events_sender,
                &executable_watch,
            );
        });

        TestRemoteListener {
            remote_listen_address,
            certificate_fingerprint,
            _program_directory: Some(program_directory),
        }
    }

    /// The listener one test serves, and what a client needs to dial it.
    struct TestRemoteListener {
        /// The address the client dials.
        remote_listen_address: String,
        /// The fingerprint of the certificate the listener presents.
        certificate_fingerprint: String,
        /// The empty directory of the program path an accept loop watches,
        /// removed when the test drops this listener. `None` for a listener
        /// that serves one connection.
        _program_directory: Option<tempfile::TempDir>,
    }

    /// Dial `test_remote_listener`, expecting the certificate it presents,
    /// send `opening_frame`, and hand back the one frame it answers with.
    fn request_test_remote_listener(
        test_remote_listener: &TestRemoteListener,
        opening_frame: &RemoteClientFrame,
    ) -> RemoteServerFrame {
        let (_frame_reader, _frame_writer, response_frame) = remote_wire::open_remote_connection(
            &test_remote_listener.remote_listen_address,
            &test_remote_listener.certificate_fingerprint,
            opening_frame,
            REMOTE_DIAL_TIMEOUT_DURATION,
            None,
        )
        .expect("the listener answers the opening frame");
        response_frame
    }

    /// A Hello naming the remote protocol versions `minimum_remote_version` to
    /// `maximum_remote_version`, the session protocol versions this build
    /// speaks, and the secret `connection_token_text`.
    fn build_remote_hello(
        minimum_remote_version: u32,
        maximum_remote_version: u32,
        connection_token_text: &str,
    ) -> RemoteClientFrame {
        RemoteClientFrame::Hello {
            minimum_remote_version,
            maximum_remote_version,
            minimum_protocol_version: MIN_PROTOCOL_VERSION,
            maximum_protocol_version: PROTOCOL_VERSION,
            connection_token: ConnectionToken::from_secret(connection_token_text),
        }
    }

    #[test]
    fn a_caller_speaking_no_remote_protocol_version_this_build_speaks_is_told_both_ranges() {
        // This dispatcher refuses every secret, and the answer still names the
        // ranges: the version is checked before the secret.
        let test_remote_listener = start_test_remote_listener(false);

        let response_frame = request_test_remote_listener(
            &test_remote_listener,
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
                remote_protocol_version: None,
            }
        );
    }

    #[test]
    fn a_caller_opening_with_the_hello_of_koshi_0_4_0_is_told_both_ranges() {
        // This dispatcher admits every secret, and the answer is still the
        // version refusal: no secret is read from that Hello.
        let test_remote_listener = start_test_remote_listener(true);
        let (mut tls_reader, mut tls_writer) = tls::connect_tls_stream(
            &test_remote_listener.remote_listen_address,
            &test_remote_listener.certificate_fingerprint,
            REMOTE_DIAL_TIMEOUT_DURATION,
        )
        .expect("the listener finishes the TLS handshake");

        tls_writer
            .write_all(&build_remote_client_frame_bytes(
                &build_previous_release_remote_hello_json(),
            ))
            .expect("the Hello is written");
        let mut answer_frame_bytes = vec![0u8; 4];
        tls_reader
            .read_exact(&mut answer_frame_bytes)
            .expect("the listener answers with a length");
        let payload_byte_count =
            u32::from_be_bytes(answer_frame_bytes[..4].try_into().expect("a 4-byte length"))
                as usize;
        answer_frame_bytes.resize(4 + payload_byte_count, 0);
        tls_reader
            .read_exact(&mut answer_frame_bytes[4..])
            .expect("the listener answers with a whole frame");

        assert_eq!(
            parse_remote_server_frames(&answer_frame_bytes),
            vec![RemoteServerFrame::Refused {
                message: "the caller speaks remote protocol versions 1 to 1, this koshi speaks \
                          2 to 2"
                    .to_string(),
                remote_protocol_version: None,
            }]
        );
    }

    #[test]
    fn a_secret_the_dispatcher_refuses_reads_as_the_sentence_and_the_settled_version() {
        let test_remote_listener = start_test_remote_listener(false);

        let response_frame = request_test_remote_listener(
            &test_remote_listener,
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
                remote_protocol_version: Some(REMOTE_PROTOCOL_VERSION),
            }
        );
    }

    #[test]
    fn an_opening_frame_that_is_not_a_hello_is_refused_with_no_secret_presented() {
        // This dispatcher admits every secret; a List arriving first is still
        // refused.
        let test_remote_listener = start_test_remote_listener(true);

        let response_frame =
            request_test_remote_listener(&test_remote_listener, &RemoteClientFrame::List);

        assert_eq!(
            response_frame,
            RemoteServerFrame::Refused {
                message: REMOTE_REFUSED.to_string(),
                remote_protocol_version: None,
            }
        );
    }

    #[test]
    fn admitted_callers_from_one_address_never_use_up_its_limit() {
        let test_remote_listener = start_test_remote_accept_loop(true);

        for dial_index in 1..=2 * MAX_UNADMITTED_ATTEMPT_COUNT {
            let response_frame = request_test_remote_listener(
                &test_remote_listener,
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
                },
                "dial {dial_index} is welcomed"
            );
        }
    }

    #[test]
    fn callers_refused_admission_use_up_their_address_limit() {
        let test_remote_listener = start_test_remote_accept_loop(false);
        let wrong_secret_hello = build_remote_hello(
            MIN_REMOTE_PROTOCOL_VERSION,
            REMOTE_PROTOCOL_VERSION,
            "wrongSecret",
        );
        for _ in 1..=MAX_UNADMITTED_ATTEMPT_COUNT {
            assert_eq!(
                request_test_remote_listener(&test_remote_listener, &wrong_secret_hello),
                RemoteServerFrame::Refused {
                    message: REMOTE_REFUSED.to_string(),
                    remote_protocol_version: Some(REMOTE_PROTOCOL_VERSION),
                }
            );
        }

        let dropped_dial = remote_wire::open_remote_connection(
            &test_remote_listener.remote_listen_address,
            &test_remote_listener.certificate_fingerprint,
            &wrong_secret_hello,
            REMOTE_DIAL_TIMEOUT_DURATION,
            None,
        );

        let server_address = match dropped_dial {
            Err(IpcError::TlsHandshakeClosed { server_address }) => server_address,
            Err(dial_error) => panic!(
                "the dial past the limit is closed before its TLS handshake finishes, \
                 not refused with: {dial_error:?}"
            ),
            Ok((_, _, response_frame)) => panic!(
                "the dial past the limit is closed before its TLS handshake finishes, \
                 not answered with {response_frame:?}"
            ),
        };
        assert_eq!(server_address, test_remote_listener.remote_listen_address);
    }

    #[test]
    fn an_admitted_caller_is_welcomed_with_the_remote_protocol_version_both_ends_speak() {
        let test_remote_listener = start_test_remote_listener(true);

        let response_frame = request_test_remote_listener(
            &test_remote_listener,
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
    //! One remote client served by [`serve_remote_connection`] over real TLS on
    //! loopback, bridged to a real session server, held open while the session
    //! keeps changing. The painted update that shows each change reaches the
    //! remote client inside `EVENT_DELIVERY_TIMEOUT_DURATION`, and the
    //! connection stays up the whole time.
    //!
    //! Also the time limits on a served connection: an admitted client that
    //! sends nothing is cut after `ADMITTED_FRAME_TIMEOUT_DURATION`, and a
    //! bridged client that sends nothing, or takes no byte, for the bridge
    //! timeout its Attach names is cut. An Attach naming the largest bridge
    //! timeout is bridged, and a bridge whose Attach names none still answers
    //! after a silence past `ADMITTED_FRAME_TIMEOUT_DURATION`.

    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread::JoinHandle;
    use std::time::SystemTime;

    use super::*;

    use koshi_core::action::ActionReference;
    use koshi_core::command::{
        Command, CommandEnvelope, CommandResult, CommandSource, FocusTabArgs, NewTabArgs, TabTarget,
    };
    use koshi_core::event::Event;
    use koshi_core::geometry::Size;
    use koshi_core::ids::{CommandId, PaneId};
    use koshi_core::key::{Key, KeyEventKind, KeyIdentity, KeyInput, KeyModifierFlags};
    use koshi_ipc::endpoint::compute_socket_address;
    use koshi_ipc::error::IpcError;
    use koshi_ipc::event::SessionEvent;
    use koshi_ipc::protocol::{IpcResponse, IpcResult};
    use koshi_ipc::router::SessionSelector;
    use koshi_ipc::transport::{FrameReader, FrameWriter, Listener};
    use koshi_link::remote_client;
    use koshi_pty::backend::state::PtyBackend;
    use koshi_runtime::ipc_server::IpcServer;
    use koshi_runtime::runtime::driver::DispatchPassOutcome;
    use koshi_runtime::runtime::event::RuntimeEvent;
    use koshi_runtime::runtime::painted_update::{PaintedUpdateReceiver, SessionFrame};
    use koshi_runtime::runtime::pty_inbox::InboxSink;
    use koshi_runtime::server::Server;
    use koshi_test_support::fake_pty::FakePtyBackend;
    use koshi_test_support::fixtures::{
        build_paint_report_request, build_test_runtime_directory, send_committed_request,
        send_committed_request_through_halves,
    };
    use koshi_test_support::throttle::pump_throttled;
    use tempfile::TempDir;

    /// How long a test waits for the session server to advertise its socket,
    /// and for the session to spawn its pane: 20 s.
    const SESSION_SERVER_START_TIMEOUT_DURATION: Duration = Duration::from_secs(20);

    /// How long a test sleeps between two checks while it waits: 10 ms.
    const SESSION_SERVER_POLL_INTERVAL_DURATION: Duration = Duration::from_millis(10);

    /// The viewport the test session starts with and attaches at: 80 columns
    /// by 24 rows.
    const TEST_VIEWPORT_SIZE: Size = Size {
        column_count: 80,
        row_count: 24,
    };

    /// How long the link is held open and driven: 4 s.
    const LINK_HOLD_DURATION: Duration = Duration::from_secs(4);

    /// How long one frame the session sends has to reach the remote client: 3 s.
    const EVENT_DELIVERY_TIMEOUT_DURATION: Duration = Duration::from_secs(3);

    /// How long the pane's writes stay unchanged before they are read: 300 ms.
    const PANE_WRITE_SETTLE_DURATION: Duration = Duration::from_millis(300);

    /// The link timeout a test client names when the bridge must outlive the
    /// test: 30 s, the default link timeout of a viewer.
    const VIEWER_LINK_TIMEOUT_DURATION: Duration = Duration::from_secs(30);

    /// The link timeout a test client names when the test waits for the
    /// bridge to end: 1 s, the least link timeout the listener takes.
    const SHORT_LINK_TIMEOUT_DURATION: Duration = Duration::from_secs(1);

    /// How long a test waits for a bridge to end once its timeout can pass:
    /// 10 s.
    const BRIDGE_END_TIMEOUT_DURATION: Duration = Duration::from_secs(10);

    /// The pause between two pings a test client sends: 100 ms.
    const TEST_PING_PAUSE_DURATION: Duration = Duration::from_millis(100);

    /// The most bytes the throttled relay carries from the client per
    /// [`THROTTLED_TIME_SLICE_DURATION`]: 1 KiB, so a 16 KiB TLS record takes
    /// 1.6 s to cross.
    const THROTTLED_SLICE_BYTE_COUNT: usize = 1024;

    /// One time slice of the throttled relay: 100 ms.
    const THROTTLED_TIME_SLICE_DURATION: Duration = Duration::from_millis(100);

    /// The most bytes the throttled relay carries back to the client per read,
    /// with no pause between reads: 64 KiB.
    const UNTHROTTLED_SLICE_BYTE_COUNT: usize = 64 * 1024;

    /// The size of the paste a test client sends over the throttled relay:
    /// 32 KiB, two whole TLS records and part of a third.
    const SLOW_PASTE_BYTE_COUNT: usize = 32 * 1024;

    /// How much sooner than its socket timeout a test accepts that a timed
    /// socket call ended: 100 ms. A timed socket call on Windows can end
    /// before its timeout.
    const MAX_EARLY_TIMEOUT_DURATION: Duration = Duration::from_millis(100);

    /// How much longer than a socket timeout a test client stays silent: 1 s.
    const SILENCE_PAST_TIMEOUT_DURATION: Duration = Duration::from_secs(1);

    /// How many bytes [`FloodingSession`] writes in one write: 64 KiB.
    const FLOOD_CHUNK_BYTE_COUNT: usize = 64 * 1024;

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
            let (runtime_event_sender, runtime_event_receiver) = mpsc::channel();
            let fake_pty_backend = Arc::new(FakePtyBackend::with_pty_sink(Arc::new(
                InboxSink::from_event_sender(runtime_event_sender.clone()),
            )));

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

        fn get_session_endpoint_path(&self) -> PathBuf {
            EndpointFile::resolve_endpoint_file_path(self.runtime_directory.path(), self.session_id)
        }

        /// This session as a remote attach locates it: its runtime directory and
        /// its id.
        fn get_located_session(&self) -> LocatedSession {
            LocatedSession {
                runtime_directory: self.runtime_directory.path().to_path_buf(),
                session_id: self.session_id,
            }
        }

        /// The id of the session's first pane, once the session has spawned it.
        ///
        /// # Panics
        ///
        /// Panics when the session has not spawned its pane within
        /// [`SESSION_SERVER_START_TIMEOUT_DURATION`].
        fn wait_for_spawned_pane_id(&self) -> PaneId {
            let wait_deadline = Instant::now() + SESSION_SERVER_START_TIMEOUT_DURATION;
            loop {
                if let Some(pane_id) = self.fake_pty_backend.list_spawned_pane_ids().first() {
                    return *pane_id;
                }
                assert!(
                    Instant::now() < wait_deadline,
                    "the session never spawned a pane"
                );
                std::thread::sleep(SESSION_SERVER_POLL_INTERVAL_DURATION);
            }
        }

        /// The bytes the session's only pane has been written, once the pane
        /// has had at least one write and the writes stop changing for
        /// `PANE_WRITE_SETTLE_DURATION`.
        ///
        /// # Panics
        ///
        /// Panics when the session has not spawned its pane, or the pane has
        /// had no write, within [`SESSION_SERVER_START_TIMEOUT_DURATION`].
        fn wait_for_settled_pane_write_bytes(&self) -> Vec<Vec<u8>> {
            let wait_deadline = Instant::now() + SESSION_SERVER_START_TIMEOUT_DURATION;
            let pane_id = self.wait_for_spawned_pane_id();
            let mut settled_write_bytes = Vec::new();
            loop {
                std::thread::sleep(PANE_WRITE_SETTLE_DURATION);
                let write_bytes = self
                    .fake_pty_backend
                    .list_pane_write_bytes(pane_id)
                    .expect("the pane is spawned");
                if !write_bytes.is_empty() && write_bytes == settled_write_bytes {
                    return settled_write_bytes;
                }
                assert!(Instant::now() < wait_deadline, "the pane had no write");
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

    /// A stand-in session server in its own runtime directory, behind the
    /// bridge. It accepts the one connection the router's bridge opens, reads
    /// and drops every byte that connection carries, and writes
    /// [`FLOOD_CHUNK_BYTE_COUNT`] bytes of `*` to it at a time until a write
    /// fails.
    struct FloodingSession {
        runtime_directory: TempDir,
        session_id: SessionId,
    }

    impl FloodingSession {
        fn start_flooding_session() -> FloodingSession {
            let runtime_directory = build_test_runtime_directory();
            let session_id = SessionId::new();
            let socket_address = compute_socket_address(runtime_directory.path(), session_id);
            let listener = Listener::bind(&socket_address).expect("bind the stand-in session");
            EndpointFile {
                socket_address,
                connection_token: ConnectionToken::generate(),
                process_id: 5000,
            }
            .write_to_path(&EndpointFile::resolve_endpoint_file_path(
                runtime_directory.path(),
                session_id,
            ))
            .expect("the endpoint file is written");
            std::thread::spawn(move || {
                let bridge_connection = listener.accept().expect("accept the router's bridge");
                let (mut bridge_reader, mut bridge_writer) = bridge_connection.split_raw();
                std::thread::spawn(move || {
                    let _ = io::copy(&mut bridge_reader, &mut io::sink());
                });
                let flood_chunk_bytes = vec![b'*'; FLOOD_CHUNK_BYTE_COUNT];
                while bridge_writer.write_all(&flood_chunk_bytes).is_ok() {}
            });
            FloodingSession {
                runtime_directory,
                session_id,
            }
        }

        /// This session as a remote attach locates it: its runtime directory and
        /// its id.
        fn get_located_session(&self) -> LocatedSession {
            LocatedSession {
                runtime_directory: self.runtime_directory.path().to_path_buf(),
                session_id: self.session_id,
            }
        }
    }

    /// Run one session server over `fake_pty_backend` until it quits: seed
    /// session `session_id` named `quiet-lake`, bind its control socket in
    /// `runtime_directory`, then serve runtime events and push frames.
    fn run_test_session_server(
        runtime_directory: &Path,
        session_id: SessionId,
        fake_pty_backend: Arc<FakePtyBackend>,
        runtime_event_receiver: mpsc::Receiver<RuntimeEvent>,
        runtime_event_sender: mpsc::Sender<RuntimeEvent>,
    ) {
        let pty_backend: Arc<dyn PtyBackend> = fake_pty_backend;
        let mut session_server =
            Server::from_runtime_parts(pty_backend, runtime_event_receiver, Arc::default());
        session_server.load_startup_config(None);
        session_server
            .bootstrap_session(
                session_id,
                "quiet-lake".to_string(),
                TEST_VIEWPORT_SIZE,
                SystemTime::now(),
            )
            .expect("the session is seeded");

        let ipc_server = IpcServer::start(
            runtime_directory,
            session_id,
            runtime_event_sender,
            Arc::clone(session_server.get_recent_event_ring()),
            None,
            None,
        )
        .expect("the control socket binds");
        session_server.attach_ipc_server(ipc_server);

        while session_server.run_dispatch_pass() == DispatchPassOutcome::Served
            && !session_server.is_quit_requested()
            && session_server.has_active_panes()
        {}
        session_server.shutdown();
    }

    /// The real remote listener one test serves, and what it reports.
    struct TestRemoteListener {
        /// The address the client dials.
        remote_listen_address: String,
        /// The fingerprint of the certificate the listener presents.
        certificate_fingerprint: String,
        /// The id of each connection the listener reports ended.
        ended_connection_id_receiver: mpsc::Receiver<u64>,
    }

    /// Serve one TLS connection on loopback with the real [`serve_remote_connection`],
    /// answering its admission questions the way the router does: the secret
    /// is admitted host-wide as connection `7`, and every locate answers
    /// `located_session`.
    fn start_test_remote_listener(located_session: LocatedSession) -> TestRemoteListener {
        let certificate_file = build_test_certificate();
        let certificate_fingerprint =
            tls::compute_certificate_fingerprint(&certificate_file.cert_der);
        let tls_config = Arc::new(
            build_remote_server_tls_config(&certificate_file).expect("the TLS config builds"),
        );

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test listener");
        let remote_listen_address = listener
            .local_addr()
            .expect("read the bound address")
            .to_string();

        let (dispatcher_events_sender, dispatcher_events_receiver) = mpsc::channel::<RouterEvent>();
        let (ended_connection_id_sender, ended_connection_id_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(router_event) = dispatcher_events_receiver.recv() {
                let RouterEvent::Admission(admission_ask) = router_event else {
                    continue;
                };
                match admission_ask {
                    AdmissionAsk::Admit {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(Some(RemoteConnectionAdmission {
                            scope: TokenScope::HostWide,
                            remote_connection_id: 7,
                        }));
                    }
                    AdmissionAsk::ListRows {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(Vec::new());
                    }
                    AdmissionAsk::Locate {
                        response_sender, ..
                    } => {
                        let _ = response_sender.send(Ok(located_session.clone()));
                    }
                    AdmissionAsk::RemoveConnection {
                        remote_connection_id,
                    } => {
                        let _ = ended_connection_id_sender.send(remote_connection_id);
                    }
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
                build_test_counted_attempt(),
            );
        });

        TestRemoteListener {
            remote_listen_address,
            certificate_fingerprint,
            ended_connection_id_receiver,
        }
    }

    /// Dial `test_remote_listener` with the test secret, expecting the
    /// certificate it presents, the way `koshi attach --remote` does, and hand
    /// back the admitted link.
    fn connect_test_remote_client(
        test_remote_listener: &TestRemoteListener,
    ) -> remote_client::RemoteLink {
        connect_test_remote_client_at(
            &test_remote_listener.remote_listen_address,
            test_remote_listener,
        )
    }

    /// Dial `server_address`, a relay in front of `test_remote_listener` or the
    /// listener itself, as [`connect_test_remote_client`] does.
    fn connect_test_remote_client_at(
        server_address: &str,
        test_remote_listener: &TestRemoteListener,
    ) -> remote_client::RemoteLink {
        remote_client::connect_remote_server(
            server_address,
            &ConnectionToken::from_secret("testSecret"),
            None,
            &test_remote_listener.certificate_fingerprint,
            Duration::from_secs(5),
            None,
        )
        .expect("the listener admits the client")
    }

    /// Relay one connection from a loopback port to `listener_address`, and
    /// return the address the client dials. The bytes from the client go on
    /// at most [`THROTTLED_SLICE_BYTE_COUNT`] per
    /// [`THROTTLED_TIME_SLICE_DURATION`]; the bytes back go on as they come.
    fn start_throttled_relay(listener_address: &str) -> String {
        let relay_listener =
            TcpListener::bind("127.0.0.1:0").expect("a loopback port for the relay");
        let relay_address = relay_listener
            .local_addr()
            .expect("read the relay's address")
            .to_string();
        let listener_address = listener_address.to_string();
        std::thread::spawn(move || {
            let (client_stream, _) = relay_listener
                .accept()
                .expect("the relay accepts the client");
            let listener_stream =
                TcpStream::connect(&listener_address).expect("the relay reaches the listener");
            let relay_deadline = Instant::now() + SESSION_SERVER_START_TIMEOUT_DURATION;
            pump_throttled(
                client_stream
                    .try_clone()
                    .expect("the client stream is shared"),
                listener_stream
                    .try_clone()
                    .expect("the listener stream is shared"),
                THROTTLED_SLICE_BYTE_COUNT,
                THROTTLED_TIME_SLICE_DURATION,
                relay_deadline,
            );
            pump_throttled(
                listener_stream,
                client_stream,
                UNTHROTTLED_SLICE_BYTE_COUNT,
                Duration::ZERO,
                relay_deadline,
            );
        });
        relay_address
    }

    /// Send the session Attach numbered 2, at [`TEST_VIEWPORT_SIZE`] and
    /// naming `link_timeout_ms`, through `writer` with
    /// [`send_committed_request_through_halves`]. The Attach answer is the
    /// next frame `reader` reads.
    ///
    /// # Panics
    ///
    /// Panics as [`send_committed_request_through_halves`] states.
    fn send_committed_attach(
        reader: &mut FrameReader,
        writer: &mut FrameWriter,
        link_timeout_ms: Option<u32>,
        graphics_capabilities: koshi_ipc::protocol::GraphicsCapabilities,
    ) {
        send_committed_request_through_halves(
            reader,
            writer,
            IpcRequest {
                request_id: 2,
                request_kind: IpcRequestKind::Attach {
                    viewport_size: TEST_VIEWPORT_SIZE,
                    resume_client_id: None,
                    resume_token: None,
                    pane_area: None,
                    graphics_capabilities,
                    cell_size: None,
                    link_timeout_ms,
                    max_frames_per_second: 60,
                    placement_preview_target: None,
                },
            },
        );
    }

    /// Submit `command` to the session over its own control connection, the
    /// way `koshi <verb>` does from outside every pane.
    fn submit_session_command(running_session: &RunningSession, command: Command) -> CommandResult {
        let session_endpoint =
            EndpointFile::load_from_path(&running_session.get_session_endpoint_path())
                .expect("the session server advertises its socket");
        let command_envelope = CommandEnvelope::from_parts(
            CommandId::new(),
            CommandSource::from_external_cli(Some(running_session.session_id), None),
            command,
        );
        submit_committed_session_request(
            &session_endpoint,
            IpcRequestKind::SubmitCommand {
                command_envelope: Box::new(command_envelope),
                fired_action: None,
            },
        )
    }

    /// Send `prepared_request_kind` to the session at `session_endpoint` over
    /// a control connection of its own: the Hello, then the request inside a
    /// `Prepare` and its `Commit`. Returns the command result that answers
    /// it.
    fn submit_committed_session_request(
        session_endpoint: &EndpointFile,
        prepared_request_kind: IpcRequestKind,
    ) -> CommandResult {
        let mut connection = Connection::connect(&session_endpoint.socket_address)
            .expect("the control socket answers");
        connection
            .send(&IpcRequest {
                request_id: 1,
                request_kind: IpcRequestKind::Hello {
                    minimum_protocol_version: koshi_ipc::protocol::MIN_PROTOCOL_VERSION,
                    maximum_protocol_version: koshi_ipc::protocol::PROTOCOL_VERSION,
                    connection_token: session_endpoint.connection_token.clone(),
                    is_remote: false,
                },
            })
            .expect("the server reads the Hello");
        let hello_response: IpcResponse = connection.recv().expect("the server answers the Hello");
        match hello_response.answer_result {
            IpcResult::Hello {
                protocol_version, ..
            } => assert_eq!(protocol_version, koshi_ipc::protocol::PROTOCOL_VERSION),
            unexpected_response => panic!("the Hello was answered with {unexpected_response:?}"),
        }

        send_committed_request(
            &mut connection,
            IpcRequest {
                request_id: 2,
                request_kind: prepared_request_kind,
            },
        );
        let command_response: IpcResponse =
            connection.recv().expect("the server answers the command");
        match command_response.answer_result {
            IpcResult::CommandResult(command_result) => command_result,
            unexpected_response => panic!("the command was answered with {unexpected_response:?}"),
        }
    }

    /// A key the remote client's keymap does not bind crosses the TLS
    /// listener, the router's bridge and the session's control socket with
    /// every field the terminal reported. The shifted key is one of those
    /// fields, and it alone decides the byte the pane reads: key `1` with
    /// Shift and shifted key `!` writes `!`, never `1`.
    #[test]
    fn a_bridged_keyboard_request_reaches_the_pane_with_the_field_that_decides_its_byte() {
        let running_session = RunningSession::start_running_session();
        let test_remote_listener =
            start_test_remote_listener(running_session.get_located_session());

        let remote_link = connect_test_remote_client(&test_remote_listener);
        let (mut reader, mut writer) = remote_client::attach_remote_session(
            remote_link,
            SessionSelector::SessionId(running_session.session_id),
            VIEWER_LINK_TIMEOUT_DURATION,
        )
        .expect("the attach request is written");

        let hello_response: IpcResponse = reader.recv().expect("the session answers the Hello");
        match hello_response.answer_result {
            IpcResult::Hello {
                protocol_version, ..
            } => assert_eq!(protocol_version, koshi_ipc::protocol::PROTOCOL_VERSION),
            unexpected_response => panic!("the Hello was answered with {unexpected_response:?}"),
        }

        send_committed_attach(
            &mut reader,
            &mut writer,
            None,
            koshi_ipc::protocol::GraphicsCapabilities::default(),
        );
        let attach_response: IpcResponse = reader.recv().expect("the session answers the attach");
        let IpcResult::Attached { .. } = attach_response.answer_result else {
            panic!(
                "expected an attach reply, got {:?}",
                attach_response.answer_result
            );
        };

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
                    key_inputs: vec![shifted_digit.clone()],
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
                    key_inputs: vec![KeyInput {
                        key_event_kind: KeyEventKind::Release,
                        ..shifted_digit
                    }],
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
    fn an_image_reaches_a_remote_client_over_tls_as_raw_chunks() {
        let running_session = RunningSession::start_running_session();
        let test_remote_listener =
            start_test_remote_listener(running_session.get_located_session());
        let remote_link = connect_test_remote_client(&test_remote_listener);
        let (mut reader, mut writer) = remote_client::attach_remote_session(
            remote_link,
            SessionSelector::SessionId(running_session.session_id),
            VIEWER_LINK_TIMEOUT_DURATION,
        )
        .expect("the attach request is written");
        let _hello_response: IpcResponse = reader.recv().expect("the session answers the Hello");
        send_committed_attach(
            &mut reader,
            &mut writer,
            None,
            koshi_ipc::protocol::GraphicsCapabilities {
                supports_kitty: true,
                supports_iterm: false,
                supports_sixel: false,
            },
        );
        let attach_response: IpcResponse = reader.recv().expect("the session answers the attach");
        let IpcResult::Attached { .. } = attach_response.answer_result else {
            panic!(
                "expected an attach reply, got {:?}",
                attach_response.answer_result
            );
        };
        let pane_id = running_session.wait_for_spawned_pane_id();

        // A 2x1 Kitty image of one red pixel and one blue pixel, in one cell.
        running_session
            .fake_pty_backend
            .push_output(
                pane_id,
                b"\x1b_Ga=T,i=7,f=32,s=2,v=1,c=1,r=1,C=1,q=2;/wAA/wAA//8=\x1b\\".to_vec(),
            )
            .expect("the pane is spawned");

        // Each painted update is reported painted, as a viewer reports each
        // update it paints, until the image's last chunk arrives.
        let mut painted_update_receiver = PaintedUpdateReceiver::new();
        let image_read_deadline = Instant::now() + EVENT_DELIVERY_TIMEOUT_DURATION;
        let mut received_rgba_bytes = Vec::new();
        loop {
            reader.set_deadline(Some(image_read_deadline));
            let received_frame = reader
                .recv_message_or_binary::<SessionEvent>()
                .expect("the image reaches the remote client in time");
            match painted_update_receiver
                .accept_session_frame(received_frame)
                .expect("a painted update part that applies")
            {
                Some(SessionFrame::Painted(received_painted_frame)) => {
                    let update_sequence = received_painted_frame.update_sequence;
                    writer
                        .send(&build_paint_report_request(update_sequence))
                        .expect("the paint report is written");
                }
                Some(SessionFrame::ImageChunk(image_chunk_frame)) => {
                    assert_eq!(
                        image_chunk_frame.get_byte_offset(),
                        received_rgba_bytes.len() as u64
                    );
                    received_rgba_bytes.extend_from_slice(image_chunk_frame.get_chunk_bytes());
                    if image_chunk_frame.is_last_chunk() {
                        break;
                    }
                }
                Some(SessionFrame::Event(_)) | None => {}
            }
        }

        assert_eq!(received_rgba_bytes, [255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn a_bridged_client_keeps_receiving_updates_while_the_link_is_held_open() {
        let running_session = RunningSession::start_running_session();
        let test_remote_listener =
            start_test_remote_listener(running_session.get_located_session());

        // Dial through the real TLS listener and attach, the way
        // `koshi attach --remote` does.
        let remote_link = connect_test_remote_client(&test_remote_listener);
        let (mut reader, mut writer) = remote_client::attach_remote_session(
            remote_link,
            SessionSelector::SessionId(running_session.session_id),
            VIEWER_LINK_TIMEOUT_DURATION,
        )
        .expect("the attach request is written");

        // The session server's own Hello answer arrives through the bridge.
        let hello_response: IpcResponse = reader.recv().expect("the session answers the Hello");
        match hello_response.answer_result {
            IpcResult::Hello {
                protocol_version, ..
            } => assert_eq!(protocol_version, koshi_ipc::protocol::PROTOCOL_VERSION),
            unexpected_response => panic!("the Hello was answered with {unexpected_response:?}"),
        }

        send_committed_attach(
            &mut reader,
            &mut writer,
            None,
            koshi_ipc::protocol::GraphicsCapabilities::default(),
        );
        let attach_response: IpcResponse = reader.recv().expect("the session answers the attach");
        let IpcResult::Attached { client_id, .. } = attach_response.answer_result else {
            panic!(
                "expected an attach reply, got {:?}",
                attach_response.answer_result
            );
        };

        // A second tab: each focus change below moves the client to the other
        // tab, and the client's next painted update shows that tab.
        let new_tab_command_response =
            submit_session_command(&running_session, Command::NewTab(NewTabArgs::default()));
        let CommandResult::Ok { .. } = new_tab_command_response else {
            panic!("the second tab was refused: {new_tab_command_response:?}");
        };

        // Drive the session for the whole hold, one focus change at a time,
        // and read the painted update that shows each one off the remote
        // connection inside `EVENT_DELIVERY_TIMEOUT_DURATION`. Each painted
        // update is applied and reported painted, as a viewer reports each
        // update it paints.
        let mut painted_update_receiver = PaintedUpdateReceiver::new();
        let hold_end_time = Instant::now() + LINK_HOLD_DURATION;
        let mut focus_round_count: u32 = 0;
        while Instant::now() < hold_end_time {
            let focus_command_response = submit_session_command(
                &running_session,
                Command::FocusTab(FocusTabArgs {
                    focus_target: TabTarget::Next,
                    client_id: Some(client_id),
                }),
            );
            let focused_tab_id = match &focus_command_response {
                CommandResult::Ok { emitted_events, .. } => match emitted_events.as_slice() {
                    [Event::TabFocused(tab_focused_event), ..] => tab_focused_event.tab_id,
                    unexpected_events => {
                        panic!("expected a focus event, got {unexpected_events:?}")
                    }
                },
                refused_focus_command_response => {
                    panic!("the focus change was refused: {refused_focus_command_response:?}")
                }
            };

            // Read frames until a painted update shows this round's tab. The
            // frames before it are read past.
            let session_event_deadline = Instant::now() + EVENT_DELIVERY_TIMEOUT_DURATION;
            loop {
                reader.set_deadline(Some(session_event_deadline));
                let received_frame = match reader.recv_message_or_binary::<SessionEvent>() {
                    Ok(received_frame) => received_frame,
                    Err(receive_error) => panic!(
                        "round {focus_round_count}: the remote stream gave no frame within \
                         {EVENT_DELIVERY_TIMEOUT_DURATION:?}: {receive_error}"
                    ),
                };
                match painted_update_receiver
                    .accept_session_frame(received_frame)
                    .expect("a painted update part that applies")
                {
                    Some(SessionFrame::Painted(received_painted_frame)) => {
                        let update_sequence = received_painted_frame.update_sequence;
                        writer
                            .send(&build_paint_report_request(update_sequence))
                            .expect("the paint report is written");
                        if received_painted_frame
                            .painted_frame
                            .client_snapshot
                            .active_tab_id
                            == focused_tab_id
                        {
                            break;
                        }
                    }
                    Some(SessionFrame::Event(_) | SessionFrame::ImageChunk(_)) | None => {}
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

    /// A viewer action that `koshi actions run` asks for crosses the session's
    /// control socket, reaches the remote viewer it names as a
    /// `ClientActionRequested` frame through the TLS listener and the
    /// router's bridge, and the viewer's `ClientActionAnswer` comes back to
    /// the caller as `Ok` under the caller's command id.
    #[test]
    fn a_viewer_action_from_the_cli_reaches_a_remote_viewer_and_its_answer_reaches_the_cli() {
        let running_session = RunningSession::start_running_session();
        let test_remote_listener =
            start_test_remote_listener(running_session.get_located_session());
        let remote_link = connect_test_remote_client(&test_remote_listener);
        let (mut reader, mut writer) = remote_client::attach_remote_session(
            remote_link,
            SessionSelector::SessionId(running_session.session_id),
            VIEWER_LINK_TIMEOUT_DURATION,
        )
        .expect("the attach request is written");
        let _hello_response: IpcResponse = reader.recv().expect("the session answers the Hello");
        send_committed_attach(
            &mut reader,
            &mut writer,
            None,
            koshi_ipc::protocol::GraphicsCapabilities::default(),
        );
        let attach_response: IpcResponse = reader.recv().expect("the session answers the attach");
        let IpcResult::Attached { client_id, .. } = attach_response.answer_result else {
            panic!(
                "expected an attach reply, got {:?}",
                attach_response.answer_result
            );
        };
        let notification_center_toggle =
            ActionReference::from_core_action_name("toggle-notification-center")
                .expect("valid name");
        let command_id = CommandId::new();
        let run_client_action_kind = IpcRequestKind::RunClientAction {
            command_id,
            command_source: CommandSource::from_external_cli(
                Some(running_session.session_id),
                Some(client_id),
            ),
            action_reference: notification_center_toggle.clone(),
        };
        let session_endpoint =
            EndpointFile::load_from_path(&running_session.get_session_endpoint_path())
                .expect("the session server advertises its socket");
        let caller_thread = std::thread::spawn(move || {
            submit_committed_session_request(&session_endpoint, run_client_action_kind)
        });

        let mut painted_update_receiver = PaintedUpdateReceiver::new();
        let session_event_deadline = Instant::now() + EVENT_DELIVERY_TIMEOUT_DURATION;
        let client_action_request_id = loop {
            reader.set_deadline(Some(session_event_deadline));
            let received_frame = reader
                .recv_message_or_binary::<SessionEvent>()
                .expect("the request reaches the remote viewer in time");
            match painted_update_receiver
                .accept_session_frame(received_frame)
                .expect("a painted update part that applies")
            {
                Some(SessionFrame::Event(SessionEvent::ClientActionRequested {
                    client_action_request_id,
                    action_reference,
                })) => {
                    assert_eq!(action_reference, notification_center_toggle);
                    break client_action_request_id;
                }
                Some(SessionFrame::Painted(received_painted_frame)) => {
                    writer
                        .send(&build_paint_report_request(
                            received_painted_frame.update_sequence,
                        ))
                        .expect("the paint report is written");
                }
                Some(SessionFrame::Event(_) | SessionFrame::ImageChunk(_)) | None => {}
            }
        };
        writer
            .send(&IpcRequest {
                request_id: 3,
                request_kind: IpcRequestKind::ClientActionAnswer {
                    client_action_request_id,
                    refusal: None,
                },
            })
            .expect("the answer is written");

        assert_eq!(
            caller_thread.join().expect("the caller thread ends"),
            CommandResult::Ok {
                command_id,
                emitted_events: Vec::new(),
            }
        );
    }

    #[test]
    fn a_silent_admitted_client_is_cut_after_the_admitted_frame_timeout_and_reported_ended() {
        // The client sends nothing after the Welcome. The connection is
        // reported ended no sooner than `ADMITTED_FRAME_TIMEOUT_DURATION`, less
        // `MAX_EARLY_TIMEOUT_DURATION`, after the dial started, and the
        // client's link reads the end of the stream.
        let test_remote_listener = start_test_remote_listener(build_located_session());

        let dial_started_at = Instant::now();
        let mut remote_link = connect_test_remote_client(&test_remote_listener);

        assert_eq!(
            test_remote_listener
                .ended_connection_id_receiver
                .recv_timeout(ADMITTED_FRAME_TIMEOUT_DURATION + BRIDGE_END_TIMEOUT_DURATION)
                .expect("the silent admitted connection is reported ended"),
            7
        );
        let connection_lifetime = dial_started_at.elapsed();
        assert!(
            connection_lifetime >= ADMITTED_FRAME_TIMEOUT_DURATION - MAX_EARLY_TIMEOUT_DURATION,
            "the connection ended {connection_lifetime:?} after the dial, before its timeout"
        );
        match remote_link.frame_reader.recv::<RemoteServerFrame>() {
            Err(IpcError::Disconnected) => {}
            unexpected_read_result => {
                panic!("the cut link read {unexpected_read_result:?}")
            }
        }
    }

    #[test]
    fn an_attach_naming_the_largest_link_timeout_is_bridged() {
        // An Attach naming 4294967295 ms, the largest a viewer names: the
        // session's Hello answer arrives through the bridge.
        let running_session = RunningSession::start_running_session();
        let test_remote_listener =
            start_test_remote_listener(running_session.get_located_session());
        let remote_link = connect_test_remote_client(&test_remote_listener);

        let (mut reader, _writer) = remote_client::attach_remote_session(
            remote_link,
            SessionSelector::SessionId(running_session.session_id),
            Duration::from_millis(u64::from(u32::MAX)),
        )
        .expect("the attach request is written");

        let hello_response: IpcResponse = reader.recv().expect("the session answers the Hello");
        match hello_response.answer_result {
            IpcResult::Hello {
                protocol_version, ..
            } => assert_eq!(protocol_version, koshi_ipc::protocol::PROTOCOL_VERSION),
            unexpected_response => panic!("the Hello was answered with {unexpected_response:?}"),
        }
    }

    #[test]
    fn an_attach_to_a_session_that_cannot_be_opened_reads_as_that_session_not_answering() {
        // The located session advertises no endpoint file and is not replacing
        // its image: the caller reads that the session did not answer, and the
        // connection is reported ended.
        let runtime_directory = build_test_runtime_directory();
        let session_id = SessionId::new();
        let test_remote_listener = start_test_remote_listener(LocatedSession {
            runtime_directory: runtime_directory.path().to_path_buf(),
            session_id,
        });
        let remote_link = connect_test_remote_client(&test_remote_listener);

        let (mut reader, _writer) = remote_client::attach_remote_session(
            remote_link,
            SessionSelector::SessionId(session_id),
            Duration::from_secs(30),
        )
        .expect("the attach request is written");
        let forwarded_answer_result = remote_client::read_forwarded_hello_answer(
            &mut reader,
            &test_remote_listener.remote_listen_address,
            &SessionSelector::SessionId(session_id),
        );

        let Err(remote_client::DialError::Refused(koshi_link::error::CliError::Runtime { detail })) =
            forwarded_answer_result
        else {
            panic!("expected a refusal, got {forwarded_answer_result:?}");
        };
        assert_eq!(
            detail,
            format!(
                "session {session_id} did not answer on the server {}",
                test_remote_listener.remote_listen_address
            )
        );
        assert_eq!(
            test_remote_listener
                .ended_connection_id_receiver
                .recv_timeout(BRIDGE_END_TIMEOUT_DURATION),
            Ok(7),
            "the connection is reported ended"
        );
    }

    /// Attach to a running session through a listener with an Attach naming
    /// `link_timeout_ms`, read the session's Hello answer, send no byte for
    /// `SILENCE_PAST_TIMEOUT_DURATION` past `ADMITTED_FRAME_TIMEOUT_DURATION`,
    /// then send a Discovery.
    ///
    /// # Panics
    ///
    /// Panics unless the session answers that Discovery, through the bridge,
    /// with its own overview.
    fn assert_discovery_after_admitted_frame_silence_is_answered(link_timeout_ms: Option<u32>) {
        let running_session = RunningSession::start_running_session();
        let test_remote_listener =
            start_test_remote_listener(running_session.get_located_session());
        let remote_client::RemoteLink {
            mut frame_reader,
            mut frame_writer,
        } = connect_test_remote_client(&test_remote_listener);
        frame_writer
            .send(&RemoteClientFrame::Attach {
                session_selector: SessionSelector::SessionId(running_session.session_id),
                link_timeout_ms,
            })
            .expect("the attach request is written");
        let _hello_response: IpcResponse =
            frame_reader.recv().expect("the session answers the Hello");

        std::thread::sleep(ADMITTED_FRAME_TIMEOUT_DURATION + SILENCE_PAST_TIMEOUT_DURATION);
        frame_writer
            .send(&IpcRequest {
                request_id: 2,
                request_kind: IpcRequestKind::Discovery,
            })
            .expect("the Discovery request is written");
        frame_reader.set_deadline(Some(Instant::now() + BRIDGE_END_TIMEOUT_DURATION));

        let discovery_response: IpcResponse = frame_reader
            .recv()
            .expect("the session answers the Discovery");
        assert_eq!(discovery_response.request_id, Some(2));
        match discovery_response.answer_result {
            IpcResult::Overview(session_overview) => assert_eq!(
                session_overview.session.session_id,
                running_session.session_id
            ),
            unexpected_response => {
                panic!("the Discovery was answered with {unexpected_response:?}")
            }
        }
    }

    #[test]
    fn a_bridge_whose_attach_names_no_timeout_answers_after_the_admitted_frame_timeout() {
        assert_discovery_after_admitted_frame_silence_is_answered(None);
    }

    #[test]
    fn a_first_request_inside_a_link_timeout_longer_than_the_admitted_frame_timeout_is_answered() {
        // 30000 ms, longer than the 20 s admitted-frame timeout.
        assert_discovery_after_admitted_frame_silence_is_answered(Some(30_000));
    }

    #[test]
    fn a_first_request_sent_past_the_link_timeout_after_the_hello_answer_reaches_the_session() {
        // The client reads the session's Hello answer, then sends its session
        // Attach `SILENCE_PAST_TIMEOUT_DURATION` past its 1 s link timeout, as a
        // client whose round trip to the server takes that long does. The
        // session's answer to that Attach arrives through the bridge.
        let running_session = RunningSession::start_running_session();
        let test_remote_listener =
            start_test_remote_listener(running_session.get_located_session());
        let remote_link = connect_test_remote_client(&test_remote_listener);
        let (mut reader, mut writer) = remote_client::attach_remote_session(
            remote_link,
            SessionSelector::SessionId(running_session.session_id),
            SHORT_LINK_TIMEOUT_DURATION,
        )
        .expect("the attach request is written");
        let _hello_response: IpcResponse = reader.recv().expect("the session answers the Hello");

        std::thread::sleep(SHORT_LINK_TIMEOUT_DURATION + SILENCE_PAST_TIMEOUT_DURATION);
        reader.set_deadline(Some(Instant::now() + BRIDGE_END_TIMEOUT_DURATION));
        send_committed_attach(
            &mut reader,
            &mut writer,
            Some(MIN_LINK_TIMEOUT_MS),
            koshi_ipc::protocol::GraphicsCapabilities::default(),
        );

        let attach_response: IpcResponse = reader.recv().expect("the session answers the attach");
        assert_eq!(attach_response.request_id, Some(2));
        match attach_response.answer_result {
            IpcResult::Attached { session_id, .. } => {
                assert_eq!(session_id, running_session.session_id);
            }
            unexpected_response => panic!("the Attach was answered with {unexpected_response:?}"),
        }
    }

    #[test]
    fn an_attach_naming_a_link_timeout_below_the_minimum_is_refused() {
        // The session is up and reachable: the link timeout of 999 ms is the
        // only reason left for the refusal.
        let running_session = RunningSession::start_running_session();
        let test_remote_listener =
            start_test_remote_listener(running_session.get_located_session());
        let remote_client::RemoteLink {
            mut frame_reader,
            mut frame_writer,
        } = connect_test_remote_client(&test_remote_listener);

        frame_writer
            .send(&RemoteClientFrame::Attach {
                session_selector: SessionSelector::SessionId(running_session.session_id),
                link_timeout_ms: Some(MIN_LINK_TIMEOUT_MS - 1),
            })
            .expect("the attach request is written");

        let refusal_frame: RemoteServerFrame = frame_reader
            .recv()
            .expect("the listener answers the attach");
        assert_eq!(
            refusal_frame,
            RemoteServerFrame::Refused {
                message: REMOTE_REFUSED.to_string(),
                remote_protocol_version: None,
            }
        );
        assert_eq!(
            test_remote_listener
                .ended_connection_id_receiver
                .recv_timeout(BRIDGE_END_TIMEOUT_DURATION)
                .expect("the refused connection is reported ended"),
            7
        );
    }

    #[test]
    fn a_bridged_client_that_sends_nothing_for_its_link_timeout_is_cut_and_reported_ended() {
        // The client sends one Discovery after the session's Hello answer, then
        // nothing, and the session sends nothing after its Discovery answer.
        // The connection is reported ended no sooner than the link timeout,
        // less `MAX_EARLY_TIMEOUT_DURATION`, after the Discovery was sent, and
        // the client's link reads the end of the stream.
        let running_session = RunningSession::start_running_session();
        let test_remote_listener =
            start_test_remote_listener(running_session.get_located_session());
        let remote_link = connect_test_remote_client(&test_remote_listener);
        let (mut reader, mut writer) = remote_client::attach_remote_session(
            remote_link,
            SessionSelector::SessionId(running_session.session_id),
            SHORT_LINK_TIMEOUT_DURATION,
        )
        .expect("the attach request is written");
        let _hello_response: IpcResponse = reader.recv().expect("the session answers the Hello");

        let discovery_sent_at = Instant::now();
        writer
            .send(&IpcRequest {
                request_id: 2,
                request_kind: IpcRequestKind::Discovery,
            })
            .expect("the Discovery request is written");
        let discovery_response: IpcResponse =
            reader.recv().expect("the session answers the Discovery");

        assert_eq!(discovery_response.request_id, Some(2));
        assert_eq!(
            test_remote_listener
                .ended_connection_id_receiver
                .recv_timeout(BRIDGE_END_TIMEOUT_DURATION)
                .expect("the silent bridge is reported ended"),
            7
        );
        let bridge_lifetime = discovery_sent_at.elapsed();
        assert!(
            bridge_lifetime >= SHORT_LINK_TIMEOUT_DURATION - MAX_EARLY_TIMEOUT_DURATION,
            "the bridge ended {bridge_lifetime:?} after the Discovery, before its timeout"
        );
        match reader.recv::<IpcResponse>() {
            Err(IpcError::Disconnected) => {}
            unexpected_read_result => {
                panic!("the cut link read {unexpected_read_result:?}")
            }
        }
    }

    #[test]
    fn a_bridged_client_that_takes_no_byte_for_its_link_timeout_is_cut_while_it_keeps_sending() {
        // The session floods the bridge, and the client reads nothing while it
        // sends a ping every `TEST_PING_PAUSE_DURATION`. The connection is
        // reported ended, and the client's next ping fails.
        let flooding_session = FloodingSession::start_flooding_session();
        let test_remote_listener =
            start_test_remote_listener(flooding_session.get_located_session());
        let remote_link = connect_test_remote_client(&test_remote_listener);
        let (_reader, mut writer) = remote_client::attach_remote_session(
            remote_link,
            SessionSelector::SessionId(flooding_session.session_id),
            SHORT_LINK_TIMEOUT_DURATION,
        )
        .expect("the attach request is written");

        let (failed_ping_sender, failed_ping_receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut ping_request_id: u64 = 1;
            loop {
                ping_request_id += 1;
                let ping_request = IpcRequest {
                    request_id: ping_request_id,
                    request_kind: IpcRequestKind::Ping,
                };
                if writer.send(&ping_request).is_err() {
                    let _ = failed_ping_sender.send(());
                    return;
                }
                std::thread::sleep(TEST_PING_PAUSE_DURATION);
            }
        });

        assert_eq!(
            test_remote_listener
                .ended_connection_id_receiver
                .recv_timeout(BRIDGE_END_TIMEOUT_DURATION)
                .expect("the bridge that takes no byte is reported ended"),
            7
        );
        failed_ping_receiver
            .recv_timeout(BRIDGE_END_TIMEOUT_DURATION)
            .expect("a ping after the cut fails");
    }

    #[test]
    fn a_paste_whose_bytes_keep_moving_keeps_the_link_past_its_link_timeout() {
        // The relay carries the client's bytes at 1 KiB every 100 ms. Each
        // 16 KiB TLS record of the paste takes 1.6 s to cross, longer than
        // the 1 s link timeout, while some bytes cross every 100 ms.
        let running_session = RunningSession::start_running_session();
        let test_remote_listener =
            start_test_remote_listener(running_session.get_located_session());
        let relay_address = start_throttled_relay(&test_remote_listener.remote_listen_address);
        let remote_link = connect_test_remote_client_at(&relay_address, &test_remote_listener);
        let (mut reader, mut writer) = remote_client::attach_remote_session(
            remote_link,
            SessionSelector::SessionId(running_session.session_id),
            SHORT_LINK_TIMEOUT_DURATION,
        )
        .expect("the attach request is written");
        let _hello_response: IpcResponse = reader.recv().expect("the session answers the Hello");
        send_committed_attach(
            &mut reader,
            &mut writer,
            Some(MIN_LINK_TIMEOUT_MS),
            koshi_ipc::protocol::GraphicsCapabilities::default(),
        );
        let attach_response: IpcResponse = reader.recv().expect("the session answers the attach");
        let IpcResult::Attached { .. } = attach_response.answer_result else {
            panic!(
                "expected an attach reply, got {:?}",
                attach_response.answer_result
            );
        };
        // The client reads every frame the session writes, as a viewer does.
        std::thread::spawn(
            move || while reader.recv_message_or_binary::<SessionEvent>().is_ok() {},
        );

        let pasted_text = "x".repeat(SLOW_PASTE_BYTE_COUNT);
        let paste_started_at = Instant::now();
        writer
            .send(&IpcRequest {
                request_id: 3,
                request_kind: IpcRequestKind::Paste {
                    pasted_text: pasted_text.clone(),
                },
            })
            .expect("the paste is written");
        // The client then sends a ping every `TEST_PING_PAUSE_DURATION`, as a
        // viewer does.
        std::thread::spawn(move || {
            let ping_request = IpcRequest {
                request_id: 4,
                request_kind: IpcRequestKind::Ping,
            };
            while writer.send(&ping_request).is_ok() {
                std::thread::sleep(TEST_PING_PAUSE_DURATION);
            }
        });
        let pane_write_bytes = running_session.wait_for_settled_pane_write_bytes();
        let paste_duration = paste_started_at.elapsed();

        assert_eq!(pane_write_bytes, vec![pasted_text.into_bytes()]);
        assert!(
            paste_duration > SHORT_LINK_TIMEOUT_DURATION * 2,
            "the paste took {paste_duration:?}, not longer than two link timeouts"
        );
        assert_eq!(
            test_remote_listener.ended_connection_id_receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty),
            "the link stayed up through the paste"
        );
    }
}

/// A writer that records what it was given and the deadline it was handed.
/// An idle timeout is taken and ignored.
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

    fn set_idle_timeout(&mut self, _idle_timeout: Option<Duration>) {}
}

#[test]
fn a_refusal_written_before_admission_never_outlives_the_admission_window() {
    // A refusal written before admission keeps the admission deadline when
    // that deadline comes first.
    let admission_deadline = Instant::now() + Duration::from_millis(1);
    let mut deadline_writer = DeadlineWriter {
        written_frame_bytes: Vec::new(),
        deadline: Some(admission_deadline),
    };

    send_refusal_with_deadline(
        &mut deadline_writer,
        compute_refusal_deadline(admission_deadline),
    );

    assert_eq!(deadline_writer.deadline, Some(admission_deadline));
    assert_eq!(
        parse_remote_server_frames(&deadline_writer.written_frame_bytes),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
            remote_protocol_version: None,
        }],
        "the refusal frame is written"
    );
}

#[test]
fn a_refusal_after_admission_gets_the_whole_refusal_window() {
    let mut deadline_writer = DeadlineWriter {
        written_frame_bytes: Vec::new(),
        deadline: None,
    };
    let refusal_start_time = Instant::now();

    send_refusal(&mut deadline_writer);

    let assigned_deadline = deadline_writer
        .deadline
        .expect("a refusal is given a deadline");
    assert!(assigned_deadline >= refusal_start_time + REFUSAL_WINDOW_DURATION);
    assert!(assigned_deadline <= Instant::now() + REFUSAL_WINDOW_DURATION);
    assert_eq!(
        parse_remote_server_frames(&deadline_writer.written_frame_bytes),
        vec![RemoteServerFrame::Refused {
            message: REMOTE_REFUSED.to_string(),
            remote_protocol_version: None,
        }]
    );
}

/// Write an empty resume file for `session_id` in `runtime_directory`, stamped
/// `age_duration` old.
fn write_aged_resume_file(runtime_directory: &Path, session_id: SessionId, age_duration: Duration) {
    std::fs::File::create(koshi_ipc::endpoint::resolve_resume_file_path(
        runtime_directory,
        session_id,
    ))
    .expect("write the resume file")
    .set_modified(std::time::SystemTime::now() - age_duration)
    .expect("age the resume file");
}

#[test]
fn a_bridge_to_a_session_replacing_its_image_opens_once_the_new_image_advertises_its_socket() {
    let runtime_directory = build_test_runtime_directory();
    let located_session = LocatedSession {
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_id: SessionId::new(),
    };
    write_aged_resume_file(
        runtime_directory.path(),
        located_session.session_id,
        Duration::ZERO,
    );
    let new_image_located_session = located_session.clone();
    let new_image_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let socket_address = koshi_ipc::endpoint::compute_socket_address(
            &new_image_located_session.runtime_directory,
            new_image_located_session.session_id,
        );
        let session_listener =
            koshi_ipc::transport::Listener::bind(&socket_address).expect("bind the new image");
        let connection_token = ConnectionToken::generate();
        EndpointFile {
            socket_address,
            connection_token: connection_token.clone(),
            process_id: std::process::id(),
        }
        .write_to_path(&EndpointFile::resolve_endpoint_file_path(
            &new_image_located_session.runtime_directory,
            new_image_located_session.session_id,
        ))
        .expect("advertise the new image");
        let mut bridged_connection = session_listener.accept().expect("accept the bridge");
        let bridged_hello: IpcRequest = bridged_connection.recv().expect("read the Hello");
        let (_new_image_reader, mut new_image_writer) = bridged_connection.split_raw();
        new_image_writer
            .write_all(b"new image")
            .expect("write to the bridge");
        (connection_token, bridged_hello, new_image_writer)
    });

    let session_bridge = open_local_session_bridge_across_restart(
        &located_session,
        (MIN_PROTOCOL_VERSION, PROTOCOL_VERSION),
    );

    let (connection_token, bridged_hello, _new_image_writer) =
        new_image_thread.join().expect("the new image thread exits");
    let (mut bridge_reader, _bridge_writer, _bridge_read_closer) =
        session_bridge.expect("the bridge opens to the new image");
    let mut bridged_bytes = [0; 9];
    bridge_reader
        .read_exact(&mut bridged_bytes)
        .expect("read from the bridge");
    assert_eq!(&bridged_bytes, b"new image");
    assert_eq!(
        bridged_hello,
        build_bridged_hello(connection_token, (MIN_PROTOCOL_VERSION, PROTOCOL_VERSION))
    );
}

#[test]
fn a_bridge_to_a_session_whose_swap_window_passes_with_nothing_advertised_does_not_open() {
    let runtime_directory = build_test_runtime_directory();
    let located_session = LocatedSession {
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_id: SessionId::new(),
    };
    let remaining_window_duration = Duration::from_millis(300);
    write_aged_resume_file(
        runtime_directory.path(),
        located_session.session_id,
        koshi_ipc::endpoint::RESTART_WINDOW_DURATION - remaining_window_duration,
    );
    let open_started_at = Instant::now();

    let session_bridge = open_local_session_bridge_across_restart(
        &located_session,
        (MIN_PROTOCOL_VERSION, PROTOCOL_VERSION),
    );

    let open_duration = open_started_at.elapsed();
    assert!(session_bridge.is_none(), "nothing is advertised");
    assert!(
        open_duration >= remaining_window_duration,
        "the open waited {open_duration:?}, less than the window left"
    );
    assert!(
        open_duration < remaining_window_duration + Duration::from_secs(5),
        "the open waited {open_duration:?}, past the window"
    );
}

#[test]
fn a_bridge_to_a_session_that_is_not_replacing_its_image_and_advertises_nothing_does_not_open() {
    let runtime_directory = build_test_runtime_directory();
    let located_session = LocatedSession {
        runtime_directory: runtime_directory.path().to_path_buf(),
        session_id: SessionId::new(),
    };

    let session_bridge = open_local_session_bridge_across_restart(
        &located_session,
        (MIN_PROTOCOL_VERSION, PROTOCOL_VERSION),
    );

    assert!(session_bridge.is_none(), "nothing is advertised");
}
