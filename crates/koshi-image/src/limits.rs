//! Limits applied to decoded image data and graphics transfers.

/// The largest decoded image, measured in pixels.
pub const MAX_IMAGE_PIXEL_COUNT: usize = 16_777_216;

/// The largest decoded RGBA buffer, measured in bytes.
pub const MAX_IMAGE_BYTE_COUNT: usize = MAX_IMAGE_PIXEL_COUNT * 4;

/// The largest encoded transfer held by one graphics sequence.
pub const MAX_GRAPHICS_TRANSFER_BYTE_COUNT: usize = 32 * 1024 * 1024;

/// The largest protocol header or command held while it is parsed.
pub const MAX_GRAPHICS_CONTROL_BYTE_COUNT: usize = 8 * 1024;

/// The largest image side accepted by a decoder.
pub const MAX_IMAGE_SIDE_PIXEL_COUNT: usize = 16_384;

/// The largest number of decoded animation frames retained in one media value.
pub const MAX_ANIMATION_FRAME_COUNT: usize = 4096;

/// The largest raw graphics prefix carried between parser instances.
pub const MAX_GRAPHICS_CARRY_BYTE_COUNT: usize = 64 * 1024;
