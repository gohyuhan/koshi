//! Unsigned LEB128 varints: seven bits per byte, low bits first, the high bit
//! set on every byte but the last. A varint takes at most
//! [`MAX_VARINT_BYTE_COUNT`] bytes.
//!
//! Example — `5` is the one byte `05`, and `300` is the two bytes `ac 02`.

/// The longest varint [`read_varint`] accepts: 10 bytes hold every `u64`.
pub const MAX_VARINT_BYTE_COUNT: usize = 10;

/// Why [`read_varint`] read no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarintError {
    /// The bytes end before a byte with the high bit clear.
    Truncated,
    /// The varint runs past [`MAX_VARINT_BYTE_COUNT`] bytes, or past the
    /// range of a `u64`.
    TooLong,
}

/// Append `varint_value` to `output_bytes` as a varint of 1 to 10 bytes.
#[inline]
pub fn write_varint(mut varint_value: u64, output_bytes: &mut Vec<u8>) {
    loop {
        let low_bits = (varint_value & 0x7f) as u8;
        varint_value >>= 7;
        if varint_value == 0 {
            output_bytes.push(low_bits);
            return;
        }
        output_bytes.push(low_bits | 0x80);
    }
}

/// Read one varint off the front of `remaining_bytes`, and move
/// `remaining_bytes` past it. `remaining_bytes` is left as it was when the
/// read fails.
///
/// # Errors
/// [`VarintError::Truncated`] when the bytes end inside the varint, and
/// [`VarintError::TooLong`] for a varint of more than 10 bytes or a value past
/// `u64::MAX`.
#[inline]
pub fn read_varint(remaining_bytes: &mut &[u8]) -> Result<u64, VarintError> {
    if let Some((&varint_byte, bytes_after_varint)) = remaining_bytes.split_first() {
        if varint_byte & 0x80 == 0 {
            *remaining_bytes = bytes_after_varint;
            return Ok(u64::from(varint_byte));
        }
    }
    read_long_varint(remaining_bytes)
}

/// [`read_varint`] for a varint of two or more bytes, and for empty input.
fn read_long_varint(remaining_bytes: &mut &[u8]) -> Result<u64, VarintError> {
    let mut varint_value = 0u64;
    for (byte_index, &varint_byte) in remaining_bytes
        .iter()
        .enumerate()
        .take(MAX_VARINT_BYTE_COUNT)
    {
        let low_bits = u64::from(varint_byte & 0x7f);
        if byte_index == MAX_VARINT_BYTE_COUNT - 1 && (low_bits > 1 || varint_byte & 0x80 != 0) {
            return Err(VarintError::TooLong);
        }
        varint_value |= low_bits << (7 * byte_index);
        if varint_byte & 0x80 == 0 {
            *remaining_bytes = &remaining_bytes[byte_index + 1..];
            return Ok(varint_value);
        }
    }
    Err(VarintError::Truncated)
}

#[cfg(test)]
mod tests;
