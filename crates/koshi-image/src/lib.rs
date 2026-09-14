//! Shared image records, validation, and raster decoding helpers.

mod animation;
mod codec;
mod error;
mod limits;
mod model;
mod serde_support;

pub use animation::{
    decode_media, AnimationError, AnimationFrame, DecodedAnimation, DecodedMedia, FrameDelay,
    LoopPolicy,
};
pub use codec::{
    compute_rgba_byte_count, decode_base64, decode_png, decode_raster, decode_raw_rgb,
    decode_raw_rgba, decompress_bounded, decompress_bounded_prefix, validate_image_dimensions,
};
pub use error::{GraphicsError, ImagePlacementError};
pub use limits::{
    MAX_ANIMATION_FRAME_COUNT, MAX_GRAPHICS_CARRY_BYTE_COUNT, MAX_GRAPHICS_CONTROL_BYTE_COUNT,
    MAX_GRAPHICS_TRANSFER_BYTE_COUNT, MAX_IMAGE_BYTE_COUNT, MAX_IMAGE_PIXEL_COUNT,
    MAX_IMAGE_SIDE_PIXEL_COUNT,
};
pub use model::{
    DecodedGraphics, DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay,
    ImageRecord, SixelBackground,
};
pub use serde_support::BoundedBytesSeed;
