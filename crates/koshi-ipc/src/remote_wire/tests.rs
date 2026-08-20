//! Tests for the frames a remote client and the machine serving it exchange
//! before any session is reached, and for what the frame decoder does with
//! bytes a caller nobody has admitted yet sent.
//!
//! On this listener the decoder's defences are what stands between a stranger
//! and this machine: the length prefix is checked before the payload buffer is
//! made, and every envelope refuses a field it does not know. The mutation
//! test below throws a fixed, repeatable stream of corrupted frames at the
//! decoder and checks three properties on each one — no panic, no payload
//! buffer made for a length past the cap, and a stream still sitting on a
//! frame boundary after a payload that arrived whole and did not decode.
//!
//! The seed is a constant, so every machine reads the same corrupted frames
//! and a failure reproduces from the run alone. The generator is xorshift64,
//! written out here rather than taken from a crate: the test needs repeatable
//! bytes, not statistical quality.

use std::io::Cursor;

use koshi_core::ids::SessionId;

use super::*;
use crate::error::IpcError;
use crate::protocol::{MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
use crate::transport::{read_message, MAX_FRAME_LEN};

/// The number every mutation run starts from, so the same corrupted frames
/// arrive on every machine and a failure reproduces from the run alone.
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
        let (outcome, _) = read_one(framed(&frame));
        assert_eq!(outcome.expect("a well-formed frame reads"), frame);
    }
}

#[test]
fn a_field_the_envelope_does_not_know_is_a_malformed_frame() {
    let payload = br#"{"List":{"extra":1}}"#;
    let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
    bytes.extend_from_slice(payload);

    let (outcome, consumed) = read_one(bytes.clone());
    let IpcError::MalformedFrame { .. } = outcome.expect_err("an unknown field is refused") else {
        panic!("an unknown field is a malformed frame");
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
        // A second, well-formed frame follows the corrupted one, so a decode
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
            // The prefix named fewer bytes than the payload holds, so a
            // whole frame arrived and its bytes do not decode.
            Err(IpcError::MalformedFrame { .. }) => {
                assert!(claimed < payload_len);
            }
            // The prefix named more bytes than the stream holds, so the
            // payload never arrived whole.
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
/// first and this build's second, so an operator reading it knows which end
/// is behind.
#[test]
fn a_doorway_version_refusal_names_the_callers_range_then_this_builds() {
    assert_eq!(
        version_refusal(2, 3),
        format!(
            "the caller speaks remote doorway 2 to 3, this koshi speaks \
             {MIN_REMOTE_PROTOCOL_VERSION} to {REMOTE_PROTOCOL_VERSION}"
        )
    );
}
