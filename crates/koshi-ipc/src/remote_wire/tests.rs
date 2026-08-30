//! Tests for the frames a remote client and the machine serving it exchange
//! before any session is reached, and for what the frame decoder does with
//! bytes a caller nobody has admitted yet sent.
//!
//! The decoder checks the length prefix before it makes the payload buffer,
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
use crate::transport::{read_message, MAX_FRAME_LEN};

/// The number every mutation run starts from.
const MUTATION_SEED: u64 = 0x5ead_bead_0f15_1234;

/// How many corrupted frames one mutation run reads.
const MUTATION_ROUNDS: usize = 2048;

/// The next number in the repeatable stream `state` names, by xorshift64.
fn next_number(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value
}

/// The Hello a real client opens with: the versions this build speaks, and a
/// secret the length every generated one has.
fn hello() -> RemoteClientFrame {
    RemoteClientFrame::Hello {
        min_remote_version: MIN_REMOTE_PROTOCOL_VERSION,
        max_remote_version: REMOTE_PROTOCOL_VERSION,
        min_protocol_version: MIN_PROTOCOL_VERSION,
        max_protocol_version: PROTOCOL_VERSION,
        token: ConnectionToken::new("7f".repeat(32)),
    }
}

/// `frame` as it travels: a 4-byte big-endian length, then the JSON payload.
fn framed(frame: &RemoteClientFrame) -> Vec<u8> {
    let payload = serde_json::to_vec(frame).expect("a remote client frame encodes");
    let length = u32::try_from(payload.len()).expect("a remote client frame fits a length prefix");
    let mut bytes = length.to_be_bytes().to_vec();
    bytes.extend_from_slice(&payload);
    bytes
}

/// Read one frame off `bytes` and report what came back alongside how many
/// bytes the read consumed.
fn read_one(bytes: Vec<u8>) -> (Result<RemoteClientFrame, IpcError>, u64) {
    let mut reader = Cursor::new(bytes);
    let outcome = read_message::<RemoteClientFrame>(&mut reader);
    let consumed = reader.position();
    (outcome, consumed)
}

/// `payload` as it travels: a 4-byte big-endian length, then the bytes as
/// given.
fn prefixed(payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).expect("a test payload fits a length prefix");
    let mut bytes = length.to_be_bytes().to_vec();
    bytes.extend_from_slice(payload);
    bytes
}

/// The one UUID every fixed id below uses.
fn fixed_uuid() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("literal UUID parses")
}

/// The malformed-frame detail `outcome` carries. Panics on any other outcome.
fn malformed_detail(outcome: Result<RemoteClientFrame, IpcError>) -> String {
    match outcome {
        Err(IpcError::MalformedFrame { detail }) => detail,
        other => panic!("expected a malformed frame, got {other:?}"),
    }
}

#[test]
fn a_well_formed_hello_reads_back_as_the_frame_that_was_written() {
    let (outcome, consumed) = read_one(framed(&hello()));
    assert_eq!(outcome.expect("a well-formed hello reads"), hello());
    assert_eq!(consumed, framed(&hello()).len() as u64);
}

#[test]
fn every_frame_a_client_opens_with_reads_back_as_itself() {
    for frame in [
        hello(),
        RemoteClientFrame::List,
        RemoteClientFrame::Attach {
            session: SessionSelector::Name("quiet-lake".to_string()),
        },
        RemoteClientFrame::Attach {
            session: SessionSelector::Id(SessionId::new()),
        },
    ] {
        let bytes = framed(&frame);
        let (outcome, consumed) = read_one(bytes.clone());
        assert_eq!(outcome.expect("a well-formed frame reads"), frame);
        assert_eq!(consumed, bytes.len() as u64);
    }
}

#[test]
fn a_payload_on_a_variant_that_carries_none_is_a_malformed_frame() {
    let payload = br#"{"List":{"extra":1}}"#;
    let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(payload);

    let (outcome, consumed) = read_one(bytes.clone());
    let IpcError::MalformedFrame { .. } = outcome.expect_err("a payload on List is refused") else {
        panic!("a payload on a variant that carries none is a malformed frame");
    };
    assert_eq!(consumed, bytes.len() as u64);
}

#[test]
fn the_cap_an_unadmitted_caller_is_held_to_is_tighter_than_the_frame_cap_and_fits_every_hello() {
    // The pre-admission cap refuses strictly more than the frame cap does.
    const {
        assert!(REMOTE_HELLO_MAX_LEN < MAX_FRAME_LEN);
    }
    let payload = framed(&hello()).len() as u32 - 4;
    assert!(
        payload < REMOTE_HELLO_MAX_LEN,
        "a hello of {payload} bytes fits the {REMOTE_HELLO_MAX_LEN}-byte pre-admission cap"
    );
}

#[test]
fn a_length_prefix_past_the_cap_is_refused_before_a_payload_buffer_is_made() {
    let mut state = MUTATION_SEED;
    for _ in 0..MUTATION_ROUNDS {
        let over = MAX_FRAME_LEN
            + 1
            + (next_number(&mut state) % u64::from(u32::MAX - MAX_FRAME_LEN)) as u32;
        let mut bytes = over.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"a payload no read ever reaches");

        let (outcome, consumed) = read_one(bytes);
        let IpcError::FrameTooLarge { len, max } =
            outcome.expect_err("a length past the cap is refused")
        else {
            panic!("a length of {over} past the cap is a frame-too-large refusal");
        };
        assert_eq!(len, u64::from(over));
        assert_eq!(max, MAX_FRAME_LEN);
        assert_eq!(consumed, 4, "only the length prefix was read");
    }
}

#[test]
fn a_corrupted_payload_leaves_the_stream_on_a_frame_boundary() {
    let mut state = MUTATION_SEED;
    let valid = framed(&hello());
    let payload_len = valid.len() - 4;
    let mut malformed = 0usize;

    for _ in 0..MUTATION_ROUNDS {
        let mut bytes = valid.clone();
        let flips = 1 + (next_number(&mut state) % 4) as usize;
        for _ in 0..flips {
            let at = 4 + (next_number(&mut state) % payload_len as u64) as usize;
            bytes[at] ^= 1 << (next_number(&mut state) % 8);
        }
        // A second, well-formed frame follows the corrupted one. A decode
        // that consumed exactly its own frame reads this one back whole.
        bytes.extend_from_slice(&valid);

        let mut reader = Cursor::new(bytes);
        match read_message::<RemoteClientFrame>(&mut reader) {
            // A flip that happens to land on a byte the frame does not care
            // about still decodes; the stream is on a boundary either way.
            Ok(_) => {}
            Err(IpcError::MalformedFrame { .. }) => malformed += 1,
            Err(other) => panic!("a whole payload that does not decode is malformed, not {other}"),
        }
        assert_eq!(
            reader.position(),
            valid.len() as u64,
            "the corrupted frame was consumed whole"
        );
        let next = read_message::<RemoteClientFrame>(&mut reader)
            .expect("the frame after a corrupted one still reads");
        assert_eq!(next, hello());
    }

    assert!(
        malformed > 0,
        "the run corrupted {MUTATION_ROUNDS} payloads and none was refused"
    );
}

#[test]
fn a_mutated_length_prefix_is_refused_or_read_and_never_panics() {
    let mut state = MUTATION_SEED;
    let valid = framed(&hello());
    let payload_len = (valid.len() - 4) as u32;
    let mut too_large = 0usize;

    for _ in 0..MUTATION_ROUNDS {
        let mut bytes = valid.clone();
        let claimed = (next_number(&mut state) % u64::from(u32::MAX)) as u32;
        bytes[..4].copy_from_slice(&claimed.to_be_bytes());

        let (outcome, consumed) = read_one(bytes);
        match outcome {
            // The prefix named exactly the payload that follows.
            Ok(frame) => {
                assert_eq!(claimed, payload_len);
                assert_eq!(frame, hello());
            }
            Err(IpcError::FrameTooLarge { len, max }) => {
                too_large += 1;
                assert!(claimed > MAX_FRAME_LEN);
                assert_eq!(len, u64::from(claimed));
                assert_eq!(max, MAX_FRAME_LEN);
                assert_eq!(consumed, 4, "only the length prefix was read");
            }
            // The prefix named fewer bytes than the payload holds: a whole
            // frame arrived and its bytes do not decode.
            Err(IpcError::MalformedFrame { .. }) => {
                assert!(claimed < payload_len);
            }
            // The prefix named more bytes than the stream holds: the payload
            // never arrived whole.
            Err(IpcError::Disconnected) => {
                assert!(claimed > payload_len);
            }
            Err(other) => panic!("a mutated length prefix never reports {other}"),
        }
    }

    assert!(
        too_large > 0,
        "the run mutated {MUTATION_ROUNDS} length prefixes and none crossed the cap"
    );
}

#[test]
fn every_refusal_a_server_sends_carries_the_one_sentence() {
    let refused = RemoteServerFrame::Refused {
        message: REMOTE_REFUSED.to_string(),
    };
    let payload = serde_json::to_vec(&refused).expect("a refusal encodes");
    let read: RemoteServerFrame = serde_json::from_slice(&payload).expect("a refusal reads");
    assert_eq!(read, refused);
    assert_eq!(REMOTE_REFUSED, "this server did not admit the connection");
}

#[test]
fn a_server_frame_reads_back_as_the_frame_that_was_written() {
    for frame in [
        RemoteServerFrame::Welcome {
            remote_version: REMOTE_PROTOCOL_VERSION,
        },
        RemoteServerFrame::Sessions {
            rows: vec![RemoteSessionRow {
                id: SessionId::new(),
                name: "quiet-lake".to_string(),
            }],
        },
    ] {
        let payload = serde_json::to_vec(&frame).expect("a server frame encodes");
        let read: RemoteServerFrame =
            serde_json::from_slice(&payload).expect("a server frame reads");
        assert_eq!(read, frame);
    }
}

/// The one refusal that is not [`REMOTE_REFUSED`] names the caller's range
/// first and this build's second.
#[test]
fn a_doorway_version_refusal_names_the_callers_range_then_this_builds() {
    assert_eq!(
        version_refusal(2, 3),
        "the caller speaks remote doorway 2 to 3, this koshi speaks 1 to 1"
    );
}

#[test]
fn this_build_speaks_remote_doorway_one_to_one() {
    assert_eq!(MIN_REMOTE_PROTOCOL_VERSION, 1);
    assert_eq!(REMOTE_PROTOCOL_VERSION, 1);
}

/// Every client frame, pinned byte for byte. The version numbers are written
/// as literals: the shape is pinned apart from what this build speaks.
#[test]
fn every_client_frame_travels_as_these_exact_bytes() {
    for (frame, bytes) in [
        (
            RemoteClientFrame::Hello {
                min_remote_version: 1,
                max_remote_version: 1,
                min_protocol_version: 2,
                max_protocol_version: 3,
                token: ConnectionToken::new("k7QxSecret"),
            },
            r#"{"Hello":{"min_remote_version":1,"max_remote_version":1,"min_protocol_version":2,"max_protocol_version":3,"token":"k7QxSecret"}}"#,
        ),
        (RemoteClientFrame::List, r#""List""#),
        (
            RemoteClientFrame::Attach {
                session: SessionSelector::Name("quiet-lake".to_string()),
            },
            r#"{"Attach":{"session":{"Name":"quiet-lake"}}}"#,
        ),
        (
            RemoteClientFrame::Attach {
                session: SessionSelector::Id(SessionId::from_uuid(fixed_uuid())),
            },
            r#"{"Attach":{"session":{"Id":"00000000-0000-0000-0000-000000000001"}}}"#,
        ),
    ] {
        assert_eq!(
            serde_json::to_string(&frame).expect("a client frame encodes"),
            bytes
        );
        assert_eq!(
            serde_json::from_str::<RemoteClientFrame>(bytes).expect("the bytes read back"),
            frame
        );
    }
}

/// Every server frame, pinned byte for byte.
#[test]
fn every_server_frame_travels_as_these_exact_bytes() {
    for (frame, bytes) in [
        (
            RemoteServerFrame::Welcome { remote_version: 1 },
            r#"{"Welcome":{"remote_version":1}}"#,
        ),
        (
            RemoteServerFrame::Refused {
                message: REMOTE_REFUSED.to_string(),
            },
            r#"{"Refused":{"message":"this server did not admit the connection"}}"#,
        ),
        (
            RemoteServerFrame::Sessions {
                rows: vec![RemoteSessionRow {
                    id: SessionId::from_uuid(fixed_uuid()),
                    name: "quiet-lake".to_string(),
                }],
            },
            r#"{"Sessions":{"rows":[{"id":"00000000-0000-0000-0000-000000000001","name":"quiet-lake"}]}}"#,
        ),
        (
            RemoteServerFrame::Sessions { rows: Vec::new() },
            r#"{"Sessions":{"rows":[]}}"#,
        ),
    ] {
        assert_eq!(
            serde_json::to_string(&frame).expect("a server frame encodes"),
            bytes
        );
        assert_eq!(
            serde_json::from_str::<RemoteServerFrame>(bytes).expect("the bytes read back"),
            frame
        );
    }
}

#[test]
fn a_field_a_struct_variant_does_not_know_is_ignored() {
    let payload = br#"{"Attach":{"session":{"Name":"quiet-lake"},"extra":1}}"#;

    let (outcome, consumed) = read_one(prefixed(payload));

    assert_eq!(
        outcome.expect("a field this build does not know is ignored"),
        RemoteClientFrame::Attach {
            session: SessionSelector::Name("quiet-lake".to_string()),
        }
    );
    assert_eq!(consumed, payload.len() as u64 + 4);
}

#[test]
fn a_misspelled_field_name_is_the_missing_field_it_displaced() {
    let payload = br#"{"Attach":{"sesion":{"Name":"quiet-lake"}}}"#;

    let (outcome, consumed) = read_one(prefixed(payload));

    assert_eq!(
        malformed_detail(outcome),
        "missing field `session` at line 1 column 42"
    );
    assert_eq!(consumed, payload.len() as u64 + 4);
}

#[test]
fn a_hello_missing_its_secret_is_a_malformed_frame() {
    let payload = br#"{"Hello":{"min_remote_version":1,"max_remote_version":1,"min_protocol_version":2,"max_protocol_version":3}}"#;

    let (outcome, consumed) = read_one(prefixed(payload));

    assert_eq!(
        malformed_detail(outcome),
        "missing field `token` at line 1 column 106"
    );
    assert_eq!(consumed, payload.len() as u64 + 4);
}

#[test]
fn a_server_frame_or_row_carrying_an_unknown_field_still_decodes() {
    let welcome =
        serde_json::from_str::<RemoteServerFrame>(r#"{"Welcome":{"remote_version":1,"extra":1}}"#)
            .expect("an unknown field on a server frame is ignored");
    assert_eq!(welcome, RemoteServerFrame::Welcome { remote_version: 1 });

    let row = serde_json::from_str::<RemoteSessionRow>(
        r#"{"id":"00000000-0000-0000-0000-000000000001","name":"quiet-lake","extra":1}"#,
    )
    .expect("an unknown field on a row is ignored");
    assert_eq!(row.name, "quiet-lake");

    let missing = serde_json::from_str::<RemoteSessionRow>(
        r#"{"id":"00000000-0000-0000-0000-000000000001","nme":"quiet-lake"}"#,
    )
    .expect_err("a misspelled name leaves the field it displaced missing");
    assert_eq!(
        missing.to_string(),
        "missing field `name` at line 1 column 64"
    );
}

#[test]
fn a_length_prefix_one_byte_short_of_the_payload_is_a_malformed_frame_read_to_that_length() {
    let mut bytes = framed(&hello());
    let claimed = (bytes.len() - 4) as u32 - 1;
    bytes[..4].copy_from_slice(&claimed.to_be_bytes());

    let (outcome, consumed) = read_one(bytes);

    assert_eq!(
        malformed_detail(outcome),
        format!("EOF while parsing an object at line 1 column {claimed}")
    );
    assert_eq!(consumed, u64::from(claimed) + 4);
}

#[test]
fn a_length_prefix_one_byte_past_the_payload_is_a_disconnect() {
    let mut bytes = framed(&hello());
    let claimed = (bytes.len() - 4) as u32 + 1;
    bytes[..4].copy_from_slice(&claimed.to_be_bytes());

    let (outcome, _) = read_one(bytes);

    let Err(IpcError::Disconnected) = outcome else {
        panic!("a payload that never arrives whole is a disconnect, got {outcome:?}");
    };
}

#[test]
fn a_zero_length_prefix_is_a_malformed_frame_that_consumed_only_the_prefix() {
    let (outcome, consumed) = read_one(prefixed(b""));

    assert_eq!(
        malformed_detail(outcome),
        "EOF while parsing a value at line 1 column 0"
    );
    assert_eq!(consumed, 4);
}

#[test]
fn a_length_prefix_exactly_at_the_cap_is_read_and_not_refused_as_too_large() {
    let bytes = MAX_FRAME_LEN.to_be_bytes().to_vec();

    let (outcome, _) = read_one(bytes);

    let Err(IpcError::Disconnected) = outcome else {
        panic!("a prefix at the cap reads its payload, got {outcome:?}");
    };
}

#[test]
fn a_length_prefix_one_past_the_cap_is_refused_as_too_large() {
    let bytes = (MAX_FRAME_LEN + 1).to_be_bytes().to_vec();

    let (outcome, consumed) = read_one(bytes);

    let Err(IpcError::FrameTooLarge { len, max }) = outcome else {
        panic!("a prefix one past the cap is refused, got {outcome:?}");
    };
    assert_eq!(len, u64::from(MAX_FRAME_LEN) + 1);
    assert_eq!(max, MAX_FRAME_LEN);
    assert_eq!(consumed, 4);
}

#[test]
fn the_largest_hello_a_generated_secret_makes_fits_the_pre_admission_cap() {
    let largest = RemoteClientFrame::Hello {
        min_remote_version: u32::MAX,
        max_remote_version: u32::MAX,
        min_protocol_version: u32::MAX,
        max_protocol_version: u32::MAX,
        token: ConnectionToken::new("7f".repeat(32)),
    };

    let payload = framed(&largest).len() as u32 - 4;

    assert_eq!(payload, 218);
    assert!(
        payload < REMOTE_HELLO_MAX_LEN,
        "the largest hello of {payload} bytes fits the {REMOTE_HELLO_MAX_LEN}-byte cap"
    );
}
