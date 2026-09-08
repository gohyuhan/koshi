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
    checked_rgba_len, decode_base64, decode_png, decode_raster, decompress_bounded,
    decompress_bounded_prefix, raw_rgb, raw_rgba, validate_dimensions,
};
pub use error::{GraphicsError, ImagePlacementError};
pub use limits::{
    MAX_ANIMATION_FRAMES, MAX_GRAPHICS_CARRY_BYTES, MAX_GRAPHICS_CONTROL_BYTES,
    MAX_GRAPHICS_TRANSFER_BYTES, MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS, MAX_IMAGE_SIDE,
};
pub use model::{
    DecodedGraphics, DecodedImage, GraphicsProtocol, ImageAction, ImageDimension, ImageDisplay,
    ImageRecord, SixelBackground,
};
pub use serde_support::BoundedBytesSeed;
