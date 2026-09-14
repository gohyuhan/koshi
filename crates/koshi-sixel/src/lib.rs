//! DEC Sixel decoding and bounded RGBA encoding.
//!
//! [`SixelParser`] decodes DCS `q` parameters and body bytes; its owner handles
//! string framing. [`SixelEncoder`] emits protocol bytes without owning the
//! terminal cursor, mode, or cleanup state.

mod capabilities;
mod encoder;
mod parser;

pub use capabilities::{
    PRIMARY_DEVICE_ATTRIBUTES_QUERY, SIXEL_GEOMETRY_QUERY, SIXEL_PALETTE_QUERY,
};
pub use encoder::{
    PreparedSixelPalette, SixelEncodeError, SixelEncodeOptions, SixelEncoder,
    DEFAULT_PALETTE_COLOR_COUNT, MAX_PALETTE_COLOR_COUNT, MAX_SIXEL_CHUNK_BYTE_COUNT,
    MAX_SIXEL_OUTPUT_BYTE_COUNT, MAX_SIXEL_TILE_BYTE_COUNT, MIN_PALETTE_COLOR_COUNT,
};
pub use parser::{
    IndexedImage, SixelGraphic, SixelPalette, SixelPaletteChange, SixelPaletteChanges, SixelParser,
    SixelPhase,
};
