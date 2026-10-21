//! Tests for the image chunk frame: the exact bytes of its head, a round trip
//! through the session plane's binary frame reader, and each decode error.

use crate::painted_update::{decode_session_binary_frame, SessionBinaryFrame};

use super::*;

/// The payload of the module doc's example: the second chunk of image 7, at
/// byte 1048576, the last one, with `chunk_bytes` after the head.
fn build_last_chunk_payload(chunk_bytes: &[u8]) -> Vec<u8> {
    let mut frame_payload = build_image_chunk_frame_head(7, 1_048_576, true).to_vec();
    frame_payload.extend_from_slice(chunk_bytes);
    frame_payload
}

#[test]
fn the_head_holds_the_kind_the_big_endian_numbers_and_the_last_chunk_flag() {
    assert_eq!(
        build_image_chunk_frame_head(7, 1_048_576, true),
        [2, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0x10, 0, 0, 1]
    );
    assert_eq!(
        build_image_chunk_frame_head(u64::MAX, 0, false),
        [2, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    );
}

#[test]
fn a_chunk_frame_reads_back_its_numbers_its_flag_and_its_bytes() {
    let decoded_frame = decode_session_binary_frame(build_last_chunk_payload(&[10, 20, 30, 40]));

    let Ok(SessionBinaryFrame::ImageChunk(image_chunk_frame)) = decoded_frame else {
        panic!("expected an image chunk, got {decoded_frame:?}");
    };
    assert_eq!(image_chunk_frame.get_image_transfer_id(), 7);
    assert_eq!(image_chunk_frame.get_byte_offset(), 1_048_576);
    assert!(image_chunk_frame.is_last_chunk());
    assert_eq!(image_chunk_frame.get_chunk_bytes(), [10, 20, 30, 40]);
}

#[test]
fn a_frame_that_holds_only_its_head_reads_as_an_empty_chunk() {
    let decoded_frame = decode_image_chunk_frame(build_last_chunk_payload(&[]));

    let Ok(image_chunk_frame) = decoded_frame else {
        panic!("expected an image chunk, got {decoded_frame:?}");
    };
    assert_eq!(image_chunk_frame.get_chunk_bytes(), [0u8; 0]);
}

#[test]
fn a_frame_shorter_than_its_head_is_truncated() {
    let frame_payload = build_last_chunk_payload(&[]);

    for frame_byte_count in 1..IMAGE_CHUNK_FRAME_HEAD_BYTE_COUNT {
        assert_eq!(
            decode_image_chunk_frame(frame_payload[..frame_byte_count].to_vec()),
            Err(SessionBinaryFrameDecodeError::Truncated),
            "{frame_byte_count} bytes"
        );
    }
}

#[test]
fn a_last_chunk_flag_other_than_zero_or_one_is_refused() {
    let mut frame_payload = build_last_chunk_payload(&[1, 2, 3, 4]);
    frame_payload[17] = 2;

    assert_eq!(
        decode_session_binary_frame(frame_payload),
        Err(SessionBinaryFrameDecodeError::ValueOutOfRange {
            field_name: "last_chunk_flag",
            decoded_value: 2,
        })
    );
}
