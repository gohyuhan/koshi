//! Kitty graphics command parsing, image decoding, and protocol output.

mod capabilities;
mod encoder;
mod parser;

pub use capabilities::{write_kitty_support_query, KITTY_QUERY_IMAGE_ID};
pub use encoder::{
    write_kitty_abort, write_kitty_delete_all, write_kitty_image_delete, write_kitty_placement,
    write_kitty_placement_delete, KittyOutputError, KittyPlacement, KittyUpload,
    KITTY_COMPRESSION_INPUT_BYTES_PER_STEP, KITTY_COMPRESSION_OUTPUT_BYTES,
    KITTY_IMAGE_CHUNKS_PER_STEP, KITTY_IMAGE_CHUNK_BYTES,
};
pub use parser::{
    parse_command, reply_display, KittyAnimationChunk, KittyAnimationCommand, KittyCommand,
    KittyCommandKind, KittyDelete,
};
pub use parser::{
    start_animation_transfer, start_transfer, KittyAnimationTransfer,
    KittyAnimationTransferOutcome, KittyChunk, KittyParser, KittyTransfer, KittyTransferOutcome,
    MAX_KITTY_CHUNK_BYTES,
};
