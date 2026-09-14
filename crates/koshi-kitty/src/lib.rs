//! Kitty graphics command parsing, image decoding, and protocol output.

mod capabilities;
mod encoder;
mod parser;

pub use capabilities::{write_kitty_support_query, KITTY_QUERY_IMAGE_ID};
pub use encoder::{
    write_kitty_abort, write_kitty_delete_all, write_kitty_image_delete, write_kitty_placement,
    write_kitty_placement_delete, write_kitty_visible_placement_delete, KittyOutputError,
    KittyPlacement, KittyUpload, KITTY_COMPRESSION_INPUT_BYTE_COUNT_PER_STEP,
    KITTY_COMPRESSION_OUTPUT_BYTE_COUNT, KITTY_IMAGE_CHUNK_BYTE_COUNT,
    KITTY_IMAGE_CHUNK_COUNT_PER_STEP,
};
pub use parser::{
    parse_kitty_command, parse_reply_display, KittyAnimationChunk, KittyAnimationCommand,
    KittyCommand, KittyCommandKind, KittyDelete,
};
pub use parser::{
    start_kitty_animation_transfer, start_kitty_transfer, KittyAnimationTransfer,
    KittyAnimationTransferOutcome, KittyChunk, KittyParser, KittyTransfer, KittyTransferOutcome,
    MAX_KITTY_CHUNK_BYTE_COUNT,
};
