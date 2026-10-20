//! Tests for [`write_varint`] and [`read_varint`]: the exact bytes of known
//! values, every length round-trips, and each malformed input names its error
//! and leaves the input unread.

use super::*;

fn encode_varint(varint_value: u64) -> Vec<u8> {
    let mut output_bytes = Vec::new();
    write_varint(varint_value, &mut output_bytes);
    output_bytes
}

#[test]
fn write_varint_writes_the_exact_bytes_of_known_values() {
    assert_eq!(encode_varint(0), [0x00]);
    assert_eq!(encode_varint(5), [0x05]);
    assert_eq!(encode_varint(127), [0x7f]);
    assert_eq!(encode_varint(128), [0x80, 0x01]);
    assert_eq!(encode_varint(300), [0xac, 0x02]);
    assert_eq!(
        encode_varint(u64::MAX),
        [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
    );
}

#[test]
fn read_varint_reads_back_every_length_and_moves_past_the_varint() {
    for shift_count in 0..64 {
        for varint_value in [1u64 << shift_count, (1u64 << shift_count) - 1] {
            let mut stream_bytes = encode_varint(varint_value);
            stream_bytes.push(0x2a);
            let mut remaining_bytes = stream_bytes.as_slice();
            assert_eq!(read_varint(&mut remaining_bytes), Ok(varint_value));
            assert_eq!(remaining_bytes, [0x2a]);
        }
    }
}

#[test]
fn read_varint_of_bytes_that_end_inside_the_varint_is_truncated_and_reads_nothing() {
    for stream_bytes in [&[][..], &[0x80][..], &[0xff, 0xff][..]] {
        let mut remaining_bytes = stream_bytes;
        assert_eq!(
            read_varint(&mut remaining_bytes),
            Err(VarintError::Truncated)
        );
        assert_eq!(remaining_bytes, stream_bytes);
    }
}

#[test]
fn read_varint_past_ten_bytes_or_past_u64_max_is_too_long_and_reads_nothing() {
    let eleven_byte_varint = [
        0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00,
    ];
    let ten_continuing_bytes = [0x80; 10];
    let past_u64_max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
    for stream_bytes in [
        &eleven_byte_varint[..],
        &ten_continuing_bytes[..],
        &past_u64_max[..],
    ] {
        let mut remaining_bytes = stream_bytes;
        assert_eq!(read_varint(&mut remaining_bytes), Err(VarintError::TooLong));
        assert_eq!(remaining_bytes, stream_bytes);
    }
}
