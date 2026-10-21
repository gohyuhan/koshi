//! Image chunks: the raw RGBA bytes of one image record, carried in binary
//! frames of kind [`IMAGE_CHUNK_FRAME_KIND`](crate::image_chunk::IMAGE_CHUNK_FRAME_KIND).
//!
//! A record a client does not hold follows the painted update that names it
//! as one [`ImageContentStart`](crate::event::SessionEvent::ImageContentStart)
//! JSON frame, then its chunks in order. Each chunk holds at most
//! [`MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT`](crate::frame::MAX_FRAME_IMAGE_CHUNK_BYTE_COUNT)
//! bytes.
//!
//! # Layout
//!
//! ```text
//! frame_kind          one byte, 2
//! image_transfer_id   8 bytes, big-endian: the content id the start named
//! byte_offset         8 bytes, big-endian: where the chunk's first byte
//!                     sits in the image's RGBA bytes
//! last_chunk_flag     one byte: 1 when the chunk ends the image, else 0
//! chunk_bytes         the raw RGBA bytes, to the end of the frame
//! ```
//!
//! Example — the second chunk of a 1.5 MiB image whose content id is 7:
//! `02`, `00 00 00 00 00 00 00 07`, `00 00 00 00 00 10 00 00`, `01`, then
//! 524288 bytes.

use crate::painted_update::SessionBinaryFrameDecodeError;

/// The kind byte of a binary frame that holds one image chunk.
pub const IMAGE_CHUNK_FRAME_KIND: u8 = 2;

/// The bytes an image chunk frame holds before its chunk bytes: the kind
/// byte, the transfer id, the byte offset and the last-chunk flag.
pub const IMAGE_CHUNK_FRAME_HEAD_BYTE_COUNT: usize = 18;

/// One image chunk, read off one binary frame by
/// [`decode_session_binary_frame`](crate::painted_update::decode_session_binary_frame).
/// It holds the frame's whole payload, and its chunk bytes are the payload
/// after the head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageChunkFrame {
    /// The image content id that the transfer's start named.
    image_transfer_id: u64,
    /// Where the chunk's first byte sits in the image's RGBA bytes.
    byte_offset: u64,
    /// Whether the chunk ends the image.
    is_last_chunk: bool,
    /// The frame's payload, its head included.
    frame_payload: Vec<u8>,
}

impl ImageChunkFrame {
    /// The image content id that the transfer's start named.
    #[must_use]
    pub fn get_image_transfer_id(&self) -> u64 {
        self.image_transfer_id
    }

    /// Where the chunk's first byte sits in the image's RGBA bytes.
    #[must_use]
    pub fn get_byte_offset(&self) -> u64 {
        self.byte_offset
    }

    /// Whether the chunk ends the image: the frame's last-chunk flag is `1`.
    #[must_use]
    pub fn is_last_chunk(&self) -> bool {
        self.is_last_chunk
    }

    /// The chunk's raw RGBA bytes: the frame's payload after its
    /// [`IMAGE_CHUNK_FRAME_HEAD_BYTE_COUNT`] bytes of head. Empty for a frame
    /// that holds only its head.
    #[must_use]
    pub fn get_chunk_bytes(&self) -> &[u8] {
        &self.frame_payload[IMAGE_CHUNK_FRAME_HEAD_BYTE_COUNT..]
    }
}

/// The head of the image chunk frame that carries the chunk at `byte_offset`
/// of the image `image_transfer_id`: the kind byte `2`, both numbers
/// big-endian, and the last-chunk flag `1` when `is_last_chunk` is set,
/// else `0`. The chunk bytes follow the head in the same frame.
#[must_use]
pub fn build_image_chunk_frame_head(
    image_transfer_id: u64,
    byte_offset: u64,
    is_last_chunk: bool,
) -> [u8; IMAGE_CHUNK_FRAME_HEAD_BYTE_COUNT] {
    let mut frame_head = [0u8; IMAGE_CHUNK_FRAME_HEAD_BYTE_COUNT];
    frame_head[0] = IMAGE_CHUNK_FRAME_KIND;
    frame_head[1..9].copy_from_slice(&image_transfer_id.to_be_bytes());
    frame_head[9..17].copy_from_slice(&byte_offset.to_be_bytes());
    frame_head[17] = u8::from(is_last_chunk);
    frame_head
}

/// Read one image chunk frame from its payload, whose first byte is the kind
/// byte. The frame keeps `frame_payload` and copies no byte of it.
///
/// # Errors
/// [`SessionBinaryFrameDecodeError::Truncated`] for a payload shorter than
/// [`IMAGE_CHUNK_FRAME_HEAD_BYTE_COUNT`], and
/// [`SessionBinaryFrameDecodeError::ValueOutOfRange`] naming
/// `last_chunk_flag` for a flag byte other than `0` and `1`.
pub(crate) fn decode_image_chunk_frame(
    frame_payload: Vec<u8>,
) -> Result<ImageChunkFrame, SessionBinaryFrameDecodeError> {
    let Some((_frame_kind, after_frame_kind)) = frame_payload.split_first() else {
        return Err(SessionBinaryFrameDecodeError::Truncated);
    };
    let Some((transfer_id_bytes, after_transfer_id)) = after_frame_kind.split_first_chunk::<8>()
    else {
        return Err(SessionBinaryFrameDecodeError::Truncated);
    };
    let Some((byte_offset_bytes, after_byte_offset)) = after_transfer_id.split_first_chunk::<8>()
    else {
        return Err(SessionBinaryFrameDecodeError::Truncated);
    };
    let Some((&last_chunk_flag, _chunk_bytes)) = after_byte_offset.split_first() else {
        return Err(SessionBinaryFrameDecodeError::Truncated);
    };
    let is_last_chunk = match last_chunk_flag {
        0 => false,
        1 => true,
        _ => {
            return Err(SessionBinaryFrameDecodeError::ValueOutOfRange {
                field_name: "last_chunk_flag",
                decoded_value: u64::from(last_chunk_flag),
            })
        }
    };
    let image_transfer_id = u64::from_be_bytes(*transfer_id_bytes);
    let byte_offset = u64::from_be_bytes(*byte_offset_bytes);
    Ok(ImageChunkFrame {
        image_transfer_id,
        byte_offset,
        is_last_chunk,
        frame_payload,
    })
}

#[cfg(test)]
mod tests;
