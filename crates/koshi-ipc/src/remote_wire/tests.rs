//! Tests for the frames a remote client and the machine serving it exchange
//! before any session is reached, and for what the frame decoder does with
//! bytes sent by a caller nobody has admitted yet.
//!
//! The decoder checks the length prefix before it makes a payload buffer,
//! and every frame ignores a field it does not know. The mutation tests
//! read a fixed, repeatable stream of corrupted frames and check three
//! properties on each one: no panic, no payload buffer made for a length past
//! the cap, and a stream still sitting on a frame boundary after a payload
//! that arrived whole and did not decode.
//!
//! The stream starts from a constant seed and steps by xorshift64, written out
//! in [`next_number`]. Every machine reads the same corrupted frames.

use std::io::Cursor;

use koshi_core::ids::SessionId;

use super::*;
use crate::error::IpcError;
use crate::protocol::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
use crate::transport::{read_message, MAX_FRAME_BYTE_COUNT};

/// The number every mutation run starts from.
const MUTATION_SEED: u64 = 0x5ead_bead_0f15_1234;

/// How many corrupted frames one mutation run reads.
const MUTATION_ROUND_COUNT: usize = 2048;

/// The next number in the repeatable stream `state` names, by xorshift64.
fn next_number(mutation_state: &mut u64) -> u64 {
    let mut next_value = *mutation_state;
    next_value ^= next_value << 13;
    next_value ^= next_value >> 7;
    next_value ^= next_value << 17;
    *mutation_state = next_value;
    next_value
}

/// The Hello a real client opens with: the versions this build speaks, and a
/// secret the length every generated one has.
fn hello() -> RemoteClientFrame {
    RemoteClientFrame::Hello {
        min_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        max_remote_version: REMOTE_PROTOCOL_VERSION,
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        connection_token: ConnectionToken::from_secret("7f".repeat(32)),
    }
}

/// `frame` as it travels: a 4-byte big-endian length, then the JSON frame payload bytes.
fn build_framed_remote_client_frame(frame: &RemoteClientFrame) -> Vec<u8> {
    let frame_payload_bytes = serde_json::to_vec(frame).expect("a remote client frame encodes");
    let frame_payload_byte_count = u32::try_from(frame_payload_bytes.len())
        .expect("a remote client frame fits a length prefix");
    let mut framed_remote_client_bytes = frame_payload_byte_count.to_be_bytes().to_vec();
    framed_remote_client_bytes.extend_from_slice(&frame_payload_bytes);
    framed_remote_client_bytes
}

/// Read one frame off `framed_input_bytes` and report what came back alongside how many
/// bytes the read consumed.
fn read_one_remote_client_frame(
    framed_input_bytes: Vec<u8>,
) -> (Result<RemoteClientFrame, IpcError>, u64) {
    let mut reader = Cursor::new(framed_input_bytes);
    let frame_read_result = read_message::<RemoteClientFrame>(&mut reader);
    let consumed_byte_count = reader.position();
    (frame_read_result, consumed_byte_count)
}

/// `frame payload bytes` as it travels: a 4-byte big-endian length, then the bytes as
/// given.
fn build_length_prefixed_payload(frame_payload_bytes: &[u8]) -> Vec<u8> {
    let frame_payload_byte_count = u32::try_from(frame_payload_bytes.len())
        .expect("a test frame payload fits a length prefix");
    let mut framed_payload_bytes = frame_payload_byte_count.to_be_bytes().to_vec();
    framed_payload_bytes.extend_from_slice(frame_payload_bytes);
    framed_payload_bytes
}

/// The one UUID every fixed id below uses.
fn build_fixed_test_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// Returns the malformed-frame detail from `frame_read_result`. Panics on any other result.
fn malformed_detail(frame_read_result: Result<RemoteClientFrame, IpcError>) -> String {
    match frame_read_result {
        Err(IpcError::MalformedFrame { error_detail }) => error_detail,
        unexpected_frame_read_result => {
            panic!("expected a malformed frame, got {unexpected_frame_read_result:?}")
        }
    }
}

#[test]
fn a_well_formed_hello_reads_back_as_the_frame_that_was_written() {
    let (frame_read_result, consumed_byte_count) =
        read_one_remote_client_frame(build_framed_remote_client_frame(&hello()));
    assert_eq!(
        frame_read_result.expect("a well-formed hello reads"),
        hello()
    );
    assert_eq!(
        consumed_byte_count,
        build_framed_remote_client_frame(&hello()).len() as u64
    );
}

#[test]
fn every_frame_a_client_opens_with_reads_back_as_itself() {
    for frame in [
        hello(),
        RemoteClientFrame::List,
        RemoteClientFrame::Attach {
            session_selector: SessionSelector::SessionName("quiet-lake".to_string()),
        },
        RemoteClientFrame::Attach {
            session_selector: SessionSelector::SessionId(SessionId::new()),
        },
    ] {
        let framed_remote_client_bytes = build_framed_remote_client_frame(&frame);
        let (frame_read_result, consumed_byte_count) =
            read_one_remote_client_frame(framed_remote_client_bytes.clone());
        assert_eq!(frame_read_result.expect("a well-formed frame reads"), frame);
        assert_eq!(consumed_byte_count, framed_remote_client_bytes.len() as u64);
    }
}

#[test]
fn a_payload_on_a_variant_that_carries_none_is_a_malformed_frame() {
    let frame_payload_bytes = br#"{"List":{"extra":1}}"#;
    let mut framed_remote_client_bytes = (frame_payload_bytes.len() as u32).to_be_bytes().to_vec();
    framed_remote_client_bytes.extend_from_slice(frame_payload_bytes);

    let (frame_read_result, consumed_byte_count) =
        read_one_remote_client_frame(framed_remote_client_bytes.clone());
    let IpcError::MalformedFrame { .. } =
        frame_read_result.expect_err("a frame payload on List is refused")
    else {
        panic!("a frame payload on a variant that carries none is a malformed frame");
    };
    assert_eq!(consumed_byte_count, framed_remote_client_bytes.len() as u64);
}

#[test]
fn the_cap_an_unadmitted_caller_is_held_to_is_tighter_than_the_frame_cap_and_fits_every_hello() {
    // The pre-admission cap refuses strictly more than the frame cap does.
    const {
        assert!(REMOTE_HELLO_MAX_BYTE_COUNT < MAX_FRAME_BYTE_COUNT);
    }
    let frame_payload_bytes = build_framed_remote_client_frame(&hello()).len() as u32 - 4;
    assert!(
        frame_payload_bytes < REMOTE_HELLO_MAX_BYTE_COUNT,
        "a hello of {frame_payload_bytes} bytes fits the {REMOTE_HELLO_MAX_BYTE_COUNT}-byte pre-admission cap"
    );
}

#[test]
fn a_length_prefix_past_the_cap_is_refused_before_a_payload_buffer_is_made() {
    let mut mutation_state = MUTATION_SEED;
    for _ in 0..MUTATION_ROUND_COUNT {
        let claimed_frame_byte_count = MAX_FRAME_BYTE_COUNT
            + 1
            + (next_number(&mut mutation_state) % u64::from(u32::MAX - MAX_FRAME_BYTE_COUNT))
                as u32;
        let mut framed_remote_client_bytes = claimed_frame_byte_count.to_be_bytes().to_vec();
        framed_remote_client_bytes.extend_from_slice(b"a frame payload no read ever reaches");

        let (frame_read_result, consumed_byte_count) =
            read_one_remote_client_frame(framed_remote_client_bytes);
        let IpcError::FrameTooLarge {
            frame_byte_count,
            maximum_frame_byte_count,
        } = frame_read_result.expect_err("a length past the cap is refused")
        else {
            panic!(
                "a length of {claimed_frame_byte_count} past the cap is a frame-too-large refusal"
            );
        };
        assert_eq!(frame_byte_count, u64::from(claimed_frame_byte_count));
        assert_eq!(maximum_frame_byte_count, MAX_FRAME_BYTE_COUNT);
        assert_eq!(consumed_byte_count, 4, "only the length prefix was read");
    }
}

#[test]
fn a_corrupted_payload_leaves_the_stream_on_a_frame_boundary() {
    let mut mutation_state = MUTATION_SEED;
    let valid_hello_frame_bytes = build_framed_remote_client_frame(&hello());
    let hello_payload_byte_count = valid_hello_frame_bytes.len() - 4;
    let mut malformed_frame_count = 0usize;

    for _ in 0..MUTATION_ROUND_COUNT {
        let mut mutated_hello_frame_bytes = valid_hello_frame_bytes.clone();
        let bit_flip_count = 1 + (next_number(&mut mutation_state) % 4) as usize;
        for _ in 0..bit_flip_count {
            let flip_byte_index =
                4 + (next_number(&mut mutation_state) % hello_payload_byte_count as u64) as usize;
            mutated_hello_frame_bytes[flip_byte_index] ^=
                1 << (next_number(&mut mutation_state) % 8);
        }
        // A second, well-formed frame follows the corrupted one. A decode
        // that consumed exactly its own frame reads this one back whole.
        mutated_hello_frame_bytes.extend_from_slice(&valid_hello_frame_bytes);

        let mut reader = Cursor::new(mutated_hello_frame_bytes);
        match read_message::<RemoteClientFrame>(&mut reader) {
            // A flip that happens to land on a byte the frame does not care
            // about still decodes; the stream is on a boundary either way.
            Ok(_) => {}
            Err(IpcError::MalformedFrame { .. }) => malformed_frame_count += 1,
            Err(unexpected_frame_read_error) => {
                panic!(
                    "a whole frame payload that does not decode is malformed, not {unexpected_frame_read_error}"
                )
            }
        }
        assert_eq!(
            reader.position(),
            valid_hello_frame_bytes.len() as u64,
            "the corrupted frame was consumed whole"
        );
        let following_frame = read_message::<RemoteClientFrame>(&mut reader)
            .expect("the frame after a corrupted one still reads");
        assert_eq!(following_frame, hello());
    }

    assert!(
        malformed_frame_count > 0,
        "the run corrupted {MUTATION_ROUND_COUNT} payloads and none was refused"
    );
}

#[test]
fn a_mutated_length_prefix_is_refused_or_read_and_never_panics() {
    let mut mutation_state = MUTATION_SEED;
    let valid_hello_frame_bytes = build_framed_remote_client_frame(&hello());
    let hello_payload_byte_count = (valid_hello_frame_bytes.len() - 4) as u32;
    let mut too_large_frame_count = 0usize;

    for _ in 0..MUTATION_ROUND_COUNT {
        let mut mutated_hello_frame_bytes = valid_hello_frame_bytes.clone();
        let claimed_frame_payload_byte_count =
            (next_number(&mut mutation_state) % u64::from(u32::MAX)) as u32;
        mutated_hello_frame_bytes[..4]
            .copy_from_slice(&claimed_frame_payload_byte_count.to_be_bytes());

        let (frame_read_result, consumed_byte_count) =
            read_one_remote_client_frame(mutated_hello_frame_bytes);
        match frame_read_result {
            // The prefix named exactly the frame payload byte count that follows.
            Ok(decoded_frame) => {
                assert_eq!(claimed_frame_payload_byte_count, hello_payload_byte_count);
                assert_eq!(decoded_frame, hello());
            }
            Err(IpcError::FrameTooLarge {
                frame_byte_count,
                maximum_frame_byte_count,
            }) => {
                too_large_frame_count += 1;
                assert!(claimed_frame_payload_byte_count > MAX_FRAME_BYTE_COUNT);
                assert_eq!(
                    frame_byte_count,
                    u64::from(claimed_frame_payload_byte_count)
                );
                assert_eq!(maximum_frame_byte_count, MAX_FRAME_BYTE_COUNT);
                assert_eq!(consumed_byte_count, 4, "only the length prefix was read");
            }
            // The prefix named fewer bytes than the frame payload holds: a whole frame
            // arrived and its bytes do not decode.
            Err(IpcError::MalformedFrame { .. }) => {
                assert!(claimed_frame_payload_byte_count < hello_payload_byte_count);
            }
            // The prefix named more bytes than the stream holds: the frame payload
            // never arrived whole.
            Err(IpcError::Disconnected) => {
                assert!(claimed_frame_payload_byte_count > hello_payload_byte_count);
            }
            Err(unexpected_frame_read_error) => {
                panic!("a mutated length prefix never reports {unexpected_frame_read_error}")
            }
        }
    }

    assert!(
        too_large_frame_count > 0,
        "the run mutated {MUTATION_ROUND_COUNT} length prefixes and none crossed the cap"
    );
}

#[test]
fn every_refusal_a_server_sends_carries_the_one_sentence() {
    let refused_server_frame = RemoteServerFrame::Refused {
        message: REMOTE_REFUSED.to_string(),
    };
    let refused_server_frame_bytes =
        serde_json::to_vec(&refused_server_frame).expect("a refusal encodes");
    let decoded_server_frame: RemoteServerFrame =
        serde_json::from_slice(&refused_server_frame_bytes).expect("a refusal reads");
    assert_eq!(decoded_server_frame, refused_server_frame);
    assert_eq!(REMOTE_REFUSED, "this server did not admit the connection");
}

#[test]
fn a_server_frame_reads_back_as_the_frame_that_was_written() {
    for frame in [
        RemoteServerFrame::Welcome {
            remote_protocol_version: REMOTE_PROTOCOL_VERSION,
        },
        RemoteServerFrame::Sessions {
            session_rows: vec![RemoteSessionRow {
                session_id: SessionId::new(),
                session_name: "quiet-lake".to_string(),
            }],
        },
    ] {
        let frame_payload_bytes = serde_json::to_vec(&frame).expect("a server frame encodes");
        let decoded_server_frame: RemoteServerFrame =
            serde_json::from_slice(&frame_payload_bytes).expect("a server frame reads");
        assert_eq!(decoded_server_frame, frame);
    }
}

/// The one refusal that is not [`REMOTE_REFUSED`] names the caller's range
/// first and this build's second.
#[test]
fn a_doorway_version_refusal_names_the_callers_range_then_this_builds() {
    assert_eq!(
        format_version_refusal(2, 3),
        "the caller speaks remote doorway 2 to 3, this koshi speaks 2 to 2"
    );
}

#[test]
fn this_build_speaks_remote_doorway_two_to_two() {
    assert_eq!(MIN_REMOTE_PROTOCOL_VERSION, 2);
    assert_eq!(REMOTE_PROTOCOL_VERSION, 2);
}

/// Every client frame, pinned byte for byte. The version numbers are written
/// as literals: the shape is pinned apart from what this build speaks.
#[test]
fn every_client_frame_travels_as_these_exact_bytes() {
    for (frame, expected_frame_json) in [
        (
            RemoteClientFrame::Hello {
                min_remote_version: 2,
                max_remote_version: 2,
                min_protocol_version: 4,
                max_protocol_version: 4,
                connection_token: ConnectionToken::from_secret("k7QxSecret"),
            },
            r#"{"Hello":{"min_remote_version":2,"max_remote_version":2,"min_protocol_version":4,"max_protocol_version":4,"connection_token":"k7QxSecret"}}"#,
        ),
        (RemoteClientFrame::List, r#""List""#),
        (
            RemoteClientFrame::Attach {
                session_selector: SessionSelector::SessionName("quiet-lake".to_string()),
            },
            r#"{"Attach":{"session_selector":{"SessionName":"quiet-lake"}}}"#,
        ),
        (
            RemoteClientFrame::Attach {
                session_selector: SessionSelector::SessionId(SessionId::from_uuid(
                    build_fixed_test_uuid(),
                )),
            },
            r#"{"Attach":{"session_selector":{"SessionId":"00000000-0000-0000-0000-000000000001"}}}"#,
        ),
    ] {
        assert_eq!(
            serde_json::to_string(&frame).expect("a client frame encodes"),
            expected_frame_json
        );
        assert_eq!(
            serde_json::from_str::<RemoteClientFrame>(expected_frame_json)
                .expect("the bytes read back"),
            frame
        );
    }
}

/// Every server frame, pinned byte for byte.
#[test]
fn every_server_frame_travels_as_these_exact_bytes() {
    for (frame, expected_frame_json) in [
        (
            RemoteServerFrame::Welcome {
                remote_protocol_version: 2,
            },
            r#"{"Welcome":{"remote_protocol_version":2}}"#,
        ),
        (
            RemoteServerFrame::Refused {
                message: REMOTE_REFUSED.to_string(),
            },
            r#"{"Refused":{"message":"this server did not admit the connection"}}"#,
        ),
        (
            RemoteServerFrame::Sessions {
                session_rows: vec![RemoteSessionRow {
                    session_id: SessionId::from_uuid(build_fixed_test_uuid()),
                    session_name: "quiet-lake".to_string(),
                }],
            },
            r#"{"Sessions":{"session_rows":[{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake"}]}}"#,
        ),
        (
            RemoteServerFrame::Sessions {
                session_rows: Vec::new(),
            },
            r#"{"Sessions":{"session_rows":[]}}"#,
        ),
    ] {
        assert_eq!(
            serde_json::to_string(&frame).expect("a server frame encodes"),
            expected_frame_json
        );
        assert_eq!(
            serde_json::from_str::<RemoteServerFrame>(expected_frame_json)
                .expect("the bytes read back"),
            frame
        );
    }
}

#[test]
fn a_field_a_struct_variant_does_not_know_is_ignored() {
    let frame_payload_bytes =
        br#"{"Attach":{"session_selector":{"SessionName":"quiet-lake"},"extra":1}}"#;

    let (frame_read_result, consumed_byte_count) =
        read_one_remote_client_frame(build_length_prefixed_payload(frame_payload_bytes));

    assert_eq!(
        frame_read_result.expect("a field this build does not know is ignored"),
        RemoteClientFrame::Attach {
            session_selector: SessionSelector::SessionName("quiet-lake".to_string()),
        }
    );
    assert_eq!(consumed_byte_count, frame_payload_bytes.len() as u64 + 4);
}

#[test]
fn a_misspelled_field_name_is_the_missing_field_it_displaced() {
    let frame_payload_bytes = br#"{"Attach":{"sesion":{"SessionName":"quiet-lake"}}}"#;

    let (frame_read_result, consumed_byte_count) =
        read_one_remote_client_frame(build_length_prefixed_payload(frame_payload_bytes));

    assert_eq!(
        malformed_detail(frame_read_result),
        "missing field `session_selector` at line 1 column 49"
    );
    assert_eq!(consumed_byte_count, frame_payload_bytes.len() as u64 + 4);
}

#[test]
fn a_hello_missing_its_secret_is_a_malformed_frame() {
    let frame_payload_bytes = br#"{"Hello":{"min_remote_version":2,"max_remote_version":2,"min_protocol_version":4,"max_protocol_version":4}}"#;

    let (frame_read_result, consumed_byte_count) =
        read_one_remote_client_frame(build_length_prefixed_payload(frame_payload_bytes));

    assert_eq!(
        malformed_detail(frame_read_result),
        "missing field `connection_token` at line 1 column 106"
    );
    assert_eq!(consumed_byte_count, frame_payload_bytes.len() as u64 + 4);
}

#[test]
fn a_server_frame_or_row_carrying_an_unknown_field_still_decodes() {
    let welcome = serde_json::from_str::<RemoteServerFrame>(
        r#"{"Welcome":{"remote_protocol_version":2,"extra":1}}"#,
    )
    .expect("an unknown field on a server frame is ignored");
    assert_eq!(
        welcome,
        RemoteServerFrame::Welcome {
            remote_protocol_version: 2
        }
    );

    let remote_session_row = serde_json::from_str::<RemoteSessionRow>(
        r#"{"session_id":"00000000-0000-0000-0000-000000000001","session_name":"quiet-lake","extra":1}"#,
    )
    .expect("an unknown field on a remote_session_row is ignored");
    assert_eq!(remote_session_row.session_name, "quiet-lake");

    let missing_field_error = serde_json::from_str::<RemoteSessionRow>(
        r#"{"session_id":"00000000-0000-0000-0000-000000000001","nme":"quiet-lake"}"#,
    )
    .expect_err("a misspelled name leaves the field it displaced missing");
    assert_eq!(
        missing_field_error.to_string(),
        "missing field `session_name` at line 1 column 72"
    );
}

#[test]
fn a_length_prefix_one_byte_short_of_the_payload_is_a_malformed_frame_read_to_that_length() {
    let mut framed_remote_client_bytes = build_framed_remote_client_frame(&hello());
    let claimed_frame_payload_byte_count = (framed_remote_client_bytes.len() - 4) as u32 - 1;
    framed_remote_client_bytes[..4]
        .copy_from_slice(&claimed_frame_payload_byte_count.to_be_bytes());

    let (frame_read_result, consumed_byte_count) =
        read_one_remote_client_frame(framed_remote_client_bytes);

    assert_eq!(
        malformed_detail(frame_read_result),
        format!("EOF while parsing an object at line 1 column {claimed_frame_payload_byte_count}")
    );
    assert_eq!(
        consumed_byte_count,
        u64::from(claimed_frame_payload_byte_count) + 4
    );
}

#[test]
fn a_length_prefix_one_byte_past_the_payload_is_a_disconnect() {
    let mut framed_remote_client_bytes = build_framed_remote_client_frame(&hello());
    let claimed_frame_payload_byte_count = (framed_remote_client_bytes.len() - 4) as u32 + 1;
    framed_remote_client_bytes[..4]
        .copy_from_slice(&claimed_frame_payload_byte_count.to_be_bytes());

    let (frame_read_result, _) = read_one_remote_client_frame(framed_remote_client_bytes);

    let Err(IpcError::Disconnected) = frame_read_result else {
        panic!(
            "a frame payload that never arrives whole is a disconnect, got {frame_read_result:?}"
        );
    };
}

#[test]
fn a_zero_length_prefix_is_a_malformed_frame_that_consumed_only_the_prefix() {
    let (frame_read_result, consumed_byte_count) =
        read_one_remote_client_frame(build_length_prefixed_payload(b""));

    assert_eq!(
        malformed_detail(frame_read_result),
        "EOF while parsing a value at line 1 column 0"
    );
    assert_eq!(consumed_byte_count, 4);
}

#[test]
fn a_length_prefix_exactly_at_the_cap_is_read_and_not_refused_as_too_large() {
    let frame_length_prefix_bytes = MAX_FRAME_BYTE_COUNT.to_be_bytes().to_vec();

    let (frame_read_result, _) = read_one_remote_client_frame(frame_length_prefix_bytes);

    let Err(IpcError::Disconnected) = frame_read_result else {
        panic!("a prefix at the cap reads its frame payload, got {frame_read_result:?}");
    };
}

#[test]
fn a_length_prefix_one_past_the_cap_is_refused_as_too_large() {
    let frame_length_prefix_bytes = (MAX_FRAME_BYTE_COUNT + 1).to_be_bytes().to_vec();

    let (frame_read_result, consumed_byte_count) =
        read_one_remote_client_frame(frame_length_prefix_bytes);

    let Err(IpcError::FrameTooLarge {
        frame_byte_count,
        maximum_frame_byte_count,
    }) = frame_read_result
    else {
        panic!("a prefix one past the cap is refused, got {frame_read_result:?}");
    };
    assert_eq!(frame_byte_count, u64::from(MAX_FRAME_BYTE_COUNT) + 1);
    assert_eq!(maximum_frame_byte_count, MAX_FRAME_BYTE_COUNT);
    assert_eq!(consumed_byte_count, 4);
}

#[test]
fn the_largest_hello_a_generated_secret_makes_fits_the_pre_admission_cap() {
    let largest_hello_frame = RemoteClientFrame::Hello {
        min_remote_version: u32::MAX,
        max_remote_version: u32::MAX,
        min_protocol_version: u32::MAX,
        max_protocol_version: u32::MAX,
        connection_token: ConnectionToken::from_secret("7f".repeat(32)),
    };

    let frame_payload_bytes =
        build_framed_remote_client_frame(&largest_hello_frame).len() as u32 - 4;

    assert_eq!(frame_payload_bytes, 229);
    assert!(
        frame_payload_bytes < REMOTE_HELLO_MAX_BYTE_COUNT,
        "the largest hello of {frame_payload_bytes} bytes fits the {REMOTE_HELLO_MAX_BYTE_COUNT}-byte cap"
    );
}
