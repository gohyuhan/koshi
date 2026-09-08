//! DEC Sixel decoding and bounded RGBA encoding.
//!
//! [`SixelParser`] consumes the payload between a DCS `q` introducer and its
//! string terminator. [`SixelEncoder`] emits protocol bytes without owning
//! terminal cursor, mode, or cleanup state.

mod capabilities;
mod encoder;
mod parser;

pub use capabilities::{
    PRIMARY_DEVICE_ATTRIBUTES_QUERY, SIXEL_GEOMETRY_QUERY, SIXEL_PALETTE_QUERY,
};
pub use encoder::{
    PreparedSixelPalette, SixelEncodeError, SixelEncodeOptions, SixelEncoder,
    DEFAULT_PALETTE_COLORS, MAX_PALETTE_COLORS, MAX_SIXEL_CHUNK_BYTES, MAX_SIXEL_OUTPUT_BYTES,
    MAX_SIXEL_TILE_BYTES, MIN_PALETTE_COLORS,
};
pub use parser::{
    IndexedImage, SixelGraphic, SixelPalette, SixelPaletteChange, SixelPaletteChanges, SixelParser,
    SixelPhase,
};
