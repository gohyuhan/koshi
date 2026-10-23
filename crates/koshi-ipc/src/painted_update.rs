//! Painted updates: how the screen that one attached client shows changes,
//! carried in binary frames.
//!
//! Every binary frame on the session plane opens with one kind byte. A part of
//! a painted update is kind
//! [`PAINTED_UPDATE_FRAME_KIND`](crate::painted_update::PAINTED_UPDATE_FRAME_KIND),
//! and an image chunk is kind
//! [`IMAGE_CHUNK_FRAME_KIND`](crate::image_chunk::IMAGE_CHUNK_FRAME_KIND)
//! ([`image_chunk`](crate::image_chunk)).
//! [`decode_session_binary_frame`](crate::painted_update::decode_session_binary_frame)
//! reads any binary frame of the session plane, and reads a kind this build
//! does not know as
//! [`SessionBinaryFrame::Unknown`](crate::painted_update::SessionBinaryFrame::Unknown):
//! the reader skips it and reads on.
//!
//! A painted update names, for each pane the client shows, the pane's cursor,
//! its scroll position, and how the rows the client holds for the pane become
//! the rows the pane shows now. The rows travel in the row encoding, and only
//! the rows the client does not hold travel at all. The update also carries the
//! [`PaintedFrame`](crate::frame::PaintedFrame): the layout, the tab bar, the
//! client's own state and everything else about each pane, as JSON. The JSON
//! travels only when its bytes differ from the JSON the client received last.
//! An update also carries the client's placement preview
//! ([`PanePlacementPreview`](crate::placement::PanePlacementPreview)) as JSON,
//! when the preview differs from the one the client holds.
//!
//! # Layout
//!
//! Every integer is an unsigned LEB128 varint ([`koshi_core::varint`]), except
//! the bytes named otherwise.
//!
//! ```text
//! frame_kind                 one byte, 1
//! part_flags                 one byte: bit 0, the part opens an update;
//!                            bit 1, the part ends it; bit 2, the update
//!                            changes the placement preview, only with bit 0
//! if the part opens an update:
//!     update_sequence        the update's number on this connection
//!     structure_byte_count   0 when the client keeps the JSON it holds
//!     structure_bytes        the PaintedFrame as JSON
//!     if the update changes the placement preview:
//!         placement_preview_byte_count   0 when the client drops the preview
//!                                        it holds
//!         placement_preview_bytes        the PanePlacementPreview as JSON
//! section_count, then each section:
//!     pane_id                16 bytes, the pane's UUID
//!     section_flags          one byte: bit 0, the section opens the pane and
//!                            its header follows; bit 1, the pane shows a
//!                            window of rows; bit 2, the client drops the rows
//!                            it holds for the pane first
//!     if the section opens the pane:
//!         cursor_row_index, cursor_column_index
//!         cursor_flags       one byte: bit 0 visible, bit 1 blinking
//!         cursor_shape       one byte: 0 none, 1 block, 2 underline, 3 bar
//!         view_top_row_index, retained_line_count
//!         if the pane shows a window:
//!             column_count, row_count, view_row_offset
//!     instruction_count, then each row instruction:
//!         0 row_count                    keep: the next rows are the rows
//!                                        the client holds at the same rows
//!         1 source_row_index row_count   copy: the next rows are the rows the
//!                                        client holds from source_row_index
//!         2 encoded_row                  new: the next row, in the row
//!                                        encoding; its first varint counts
//!                                        the bytes after it
//! ```
//!
//! An update too large for one frame travels in several parts, in order and
//! with no other frame between them. A pane whose rows do not fit one part
//! goes on in the first section of the next part: a section with
//! `section_flags` `0` that names the same pane and continues its row
//! instructions.
//!
//! Example — one keystroke in a 3-row pane that the client already holds,
//! with the JSON unchanged: one part with flags `03`, the sequence, a
//! structure byte count of `0`, one section that opens the pane with its
//! window, and the instructions `keep 1`, `new <row 1>`, `keep 1`.

use koshi_core::ids::PaneId;
use koshi_core::varint::{read_varint, write_varint, VarintError, MAX_VARINT_BYTE_COUNT};
use thiserror::Error;
use uuid::Uuid;

use crate::frame::PaintedFrame;
use crate::image_chunk::{decode_image_chunk_frame, ImageChunkFrame, IMAGE_CHUNK_FRAME_KIND};
use crate::placement::PanePlacementPreview;
use crate::transport::MAX_FRAME_BYTE_COUNT;

/// The kind byte of a binary frame that holds one part of a painted update.
pub const PAINTED_UPDATE_FRAME_KIND: u8 = 1;

const PART_OPENS_UPDATE_FLAG: u8 = 1 << 0;
const PART_ENDS_UPDATE_FLAG: u8 = 1 << 1;
const PART_CHANGES_PLACEMENT_PREVIEW_FLAG: u8 = 1 << 2;

const SECTION_OPENS_PANE_FLAG: u8 = 1 << 0;
const SECTION_HAS_WINDOW_FLAG: u8 = 1 << 1;
const SECTION_RESETS_ROWS_FLAG: u8 = 1 << 2;

const CURSOR_VISIBLE_FLAG: u8 = 1 << 0;
const CURSOR_BLINKING_FLAG: u8 = 1 << 1;

const CURSOR_SHAPE_NONE_CODE: u8 = 0;
const CURSOR_SHAPE_BLOCK_CODE: u8 = 1;
const CURSOR_SHAPE_UNDERLINE_CODE: u8 = 2;
const CURSOR_SHAPE_BAR_CODE: u8 = 3;

const KEEP_INSTRUCTION_TAG: u8 = 0;
const COPY_INSTRUCTION_TAG: u8 = 1;
const NEW_INSTRUCTION_TAG: u8 = 2;

/// The bytes a section takes besides its flags, its pane header and its
/// instructions, at most: the pane id and the instruction count.
const SECTION_FRAME_BYTE_COUNT: usize = 16 + MAX_VARINT_BYTE_COUNT;

/// The bytes a part takes before its sections, at most, besides its opening:
/// the frame kind, the part flags, and the section count.
const PART_FRAME_BYTE_COUNT: usize = 1 + 1 + MAX_VARINT_BYTE_COUNT;

/// One binary frame of the session plane, as [`decode_session_binary_frame`]
/// reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionBinaryFrame {
    /// One part of a painted update.
    PaintedUpdatePart(PaintedUpdatePart),
    /// One chunk of an image record.
    ImageChunk(ImageChunkFrame),
    /// A frame of a kind this build does not know. The reader skips it.
    Unknown {
        /// The frame's kind byte.
        frame_kind: u8,
    },
}

/// One part of a painted update, read off one binary frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaintedUpdatePart {
    /// The update's sequence number and JSON, carried by the part that opens
    /// the update. `None` on every other part.
    pub update_opening: Option<PaintedUpdateOpening>,
    /// Whether this part ends the update.
    pub is_last_part: bool,
    /// The part's pane sections, in order.
    pub pane_sections: Vec<PaneSection>,
}

/// What the part that opens a painted update carries before its sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaintedUpdateOpening {
    /// The update's number on this connection. The first update is `1`, and
    /// each update after it is one more.
    pub update_sequence: u64,
    /// The update's [`PaintedFrame`]. `None` when its JSON equals the JSON
    /// the client received last, and the client keeps that one.
    pub painted_frame: Option<Box<PaintedFrame>>,
    /// How the update changes the placement preview the client holds.
    pub placement_preview_update: PlacementPreviewUpdate<Box<PanePlacementPreview>>,
}

/// How a painted update changes the placement preview its client holds.
/// `PreviewValue` is the preview's JSON bytes for an update being encoded, and
/// the decoded [`PanePlacementPreview`] for an update read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementPreviewUpdate<PreviewValue> {
    /// The client keeps the preview it holds, or keeps holding none.
    Unchanged,
    /// The client drops the preview it holds.
    Cleared,
    /// The client holds this preview in place of the one it holds.
    Replaced(PreviewValue),
}

/// One pane's section of one part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSection {
    /// The pane the section describes.
    pub pane_id: PaneId,
    /// The pane's header, on the section that opens the pane. `None` on a
    /// section that goes on with the row instructions of the pane that the
    /// previous part's last section opened.
    pub pane_header: Option<PaneHeader>,
    /// The section's row instructions, top row first.
    pub row_instructions: Vec<RowInstruction<Vec<u8>>>,
}

/// Everything about one pane that changes with its output: the cursor, the
/// scroll position, and the window of rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneHeader {
    /// The cursor's position and look within the pane's content area.
    pub cursor: PaneCursor,
    /// The absolute line number of the top row the pane shows, counting every
    /// line the pane has pushed into history.
    pub view_top_row_index: u64,
    /// How many history lines the pane retains.
    pub retained_line_count: u64,
    /// The pane's window of rows. `None` for a pane that shows no rows.
    pub pane_window: Option<PaneWindow>,
}

/// A pane's cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneCursor {
    /// The cursor's row within the content area, starting at 0.
    pub row_index: u16,
    /// The cursor's column within the content area, starting at 0.
    pub column_index: u16,
    /// Whether the cursor is visible.
    pub is_visible: bool,
    /// Whether the cursor blinks.
    pub is_blinking: bool,
    /// The shape the pane asked for with DECSCUSR, or `None` while it asked
    /// for none. A shape code this build does not know reads as `None`.
    pub shape: Option<PaneCursorShape>,
}

/// A cursor shape a pane asked for with DECSCUSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneCursorShape {
    /// A box filling the whole cell.
    Block,
    /// A line along the bottom of the cell.
    Underline,
    /// A vertical bar at the cell's left edge.
    Bar,
}

/// A pane's window of rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneWindow {
    /// The width of the pane's content area, in cells.
    pub column_count: u16,
    /// The rows the window shows. The pane's row instructions give exactly
    /// this many rows.
    pub row_count: u16,
    /// Rows scrolled up from the live bottom; `0` shows the live bottom.
    pub view_row_offset: u64,
    /// Whether the client drops the rows it holds for the pane before the row
    /// instructions run. A pane with this set holds no row to keep or copy.
    pub is_reset: bool,
}

/// One step that builds a pane's rows, top row first, from the rows the
/// client holds for the pane. `RowBytes` is `Vec<u8>` for a decoded update,
/// and a borrowed `&[u8]` for an update being encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowInstruction<RowBytes> {
    /// The next `row_count` rows are the rows the client holds at the same
    /// row indexes. Never 0.
    Keep {
        /// The number of rows. Never 0.
        row_count: u16,
    },
    /// The next `row_count` rows are the rows the client holds from
    /// `source_row_index` on.
    Copy {
        /// The first held row to copy.
        source_row_index: u16,
        /// The number of rows. Never 0.
        row_count: u16,
    },
    /// The next row, in the row encoding.
    New {
        /// The encoded row, its leading byte count included.
        encoded_row_bytes: RowBytes,
    },
}

/// One pane of a painted update being encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneUpdate<'row> {
    /// The pane.
    pub pane_id: PaneId,
    /// The pane's header.
    pub pane_header: PaneHeader,
    /// The pane's row instructions, top row first.
    pub row_instructions: Vec<RowInstruction<&'row [u8]>>,
}

/// Why [`encode_painted_update`] built no frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PaintedUpdateEncodeError {
    /// The JSON of the update's [`PaintedFrame`] and of its placement preview
    /// do not fit one frame with the part's own bytes.
    #[error(
        "the painted structure of {structure_byte_count} bytes and the placement preview of \
         {placement_preview_byte_count} bytes do not fit one frame"
    )]
    StructureTooLarge {
        /// The structure JSON's byte count.
        structure_byte_count: usize,
        /// The placement preview JSON's byte count, `0` when the update
        /// carries no preview.
        placement_preview_byte_count: usize,
    },
    /// One row instruction does not fit one frame with the bytes of a part
    /// and a section.
    #[error("a row of {row_byte_count} bytes does not fit one frame")]
    RowTooLarge {
        /// The encoded row's byte count.
        row_byte_count: usize,
    },
}

/// Why [`decode_session_binary_frame`] read no frame.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SessionBinaryFrameDecodeError {
    /// The frame holds no byte, so no kind byte.
    #[error("the binary frame holds no kind byte")]
    MissingFrameKind,
    /// The bytes end inside a field.
    #[error("the binary frame ends inside a field")]
    Truncated,
    /// A varint runs past 10 bytes, or past the range of a `u64`.
    #[error("a varint of the painted update runs past 10 bytes or past the range of a u64")]
    VarintTooLong,
    /// A field holds a value past its range.
    #[error("{field_name} holds {decoded_value}, past its range")]
    ValueOutOfRange {
        /// The field: `cursor_row_index`, `row_count`, …
        field_name: &'static str,
        /// The value read.
        decoded_value: u64,
    },
    /// A flag byte sets a bit this build does not know, or a section that
    /// goes on with a pane sets a flag.
    #[error("{field_name} holds the flag bits {flag_bits:#04x}, which this build does not read")]
    UnknownFlags {
        /// The field: `part_flags`, `section_flags` or `cursor_flags`.
        field_name: &'static str,
        /// The flag byte read.
        flag_bits: u8,
    },
    /// A row instruction tag is not keep, copy or new.
    #[error("row instruction tag {instruction_tag} is not keep, copy or new")]
    UnknownRowInstruction {
        /// The tag read.
        instruction_tag: u8,
    },
    /// Bytes follow the last section.
    #[error("{trailing_byte_count} bytes follow the last section")]
    TrailingBytes {
        /// The count of bytes after the last section.
        trailing_byte_count: usize,
    },
    /// The JSON of the [`PaintedFrame`] or of the [`PanePlacementPreview`]
    /// does not decode.
    #[error("the painted structure does not decode: {error_detail}")]
    MalformedStructure {
        /// The JSON decoder's message.
        error_detail: String,
    },
}

/// Encode one painted update as the payloads of its binary frames, in order.
/// Each payload is at most [`MAX_FRAME_BYTE_COUNT`] bytes. `structure_json_bytes`
/// is the update's [`PaintedFrame`] as JSON, or `None` for a client that keeps
/// the JSON it holds. `placement_preview_update` carries the client's
/// [`PanePlacementPreview`] as JSON when it replaces the one the client holds.
///
/// # Errors
/// [`PaintedUpdateEncodeError::StructureTooLarge`] for JSON that does not fit
/// one frame, and [`PaintedUpdateEncodeError::RowTooLarge`] for a row that does
/// not fit one frame. Nothing is encoded then.
pub fn encode_painted_update(
    update_sequence: u64,
    structure_json_bytes: Option<&[u8]>,
    placement_preview_update: PlacementPreviewUpdate<&[u8]>,
    pane_updates: &[PaneUpdate<'_>],
) -> Result<Vec<Vec<u8>>, PaintedUpdateEncodeError> {
    encode_painted_update_in_parts(
        update_sequence,
        structure_json_bytes,
        placement_preview_update,
        pane_updates,
        MAX_FRAME_BYTE_COUNT as usize,
    )
}

/// [`encode_painted_update`] with parts of at most `maximum_part_byte_count`
/// bytes.
fn encode_painted_update_in_parts(
    update_sequence: u64,
    structure_json_bytes: Option<&[u8]>,
    placement_preview_update: PlacementPreviewUpdate<&[u8]>,
    pane_updates: &[PaneUpdate<'_>],
    maximum_part_byte_count: usize,
) -> Result<Vec<Vec<u8>>, PaintedUpdateEncodeError> {
    let mut opening_bytes = Vec::new();
    write_varint(update_sequence, &mut opening_bytes);
    let structure_json_bytes = structure_json_bytes.unwrap_or_default();
    write_varint(structure_json_bytes.len() as u64, &mut opening_bytes);
    opening_bytes.extend_from_slice(structure_json_bytes);
    let placement_preview_json_bytes: Option<&[u8]> = match placement_preview_update {
        PlacementPreviewUpdate::Unchanged => None,
        PlacementPreviewUpdate::Cleared => Some(&[]),
        PlacementPreviewUpdate::Replaced(placement_preview_json_bytes) => {
            Some(placement_preview_json_bytes)
        }
    };
    if let Some(placement_preview_json_bytes) = placement_preview_json_bytes {
        write_varint(
            placement_preview_json_bytes.len() as u64,
            &mut opening_bytes,
        );
        opening_bytes.extend_from_slice(placement_preview_json_bytes);
    }
    if PART_FRAME_BYTE_COUNT + opening_bytes.len() > maximum_part_byte_count {
        return Err(PaintedUpdateEncodeError::StructureTooLarge {
            structure_byte_count: structure_json_bytes.len(),
            placement_preview_byte_count: placement_preview_json_bytes.map_or(0, <[u8]>::len),
        });
    }
    let mut part_writer = PartWriter {
        maximum_part_byte_count,
        part_payloads: Vec::new(),
        opening_bytes,
        opening_flags: if placement_preview_json_bytes.is_some() {
            PART_OPENS_UPDATE_FLAG | PART_CHANGES_PLACEMENT_PREVIEW_FLAG
        } else {
            PART_OPENS_UPDATE_FLAG
        },
        section_bytes: Vec::new(),
        section_count: 0,
    };
    let mut instruction_bytes = Vec::new();
    for pane_update in pane_updates {
        let mut open_section = OpenSection::from_pane_update(pane_update);
        if !part_writer.can_take_section_bytes(open_section.compute_byte_count_bound()) {
            part_writer.close_part(false);
        }
        for row_instruction in &pane_update.row_instructions {
            instruction_bytes.clear();
            encode_row_instruction(row_instruction, &mut instruction_bytes);
            if !part_writer.can_take_section_bytes(
                open_section.compute_byte_count_bound() + instruction_bytes.len(),
            ) {
                part_writer.add_section(&open_section);
                part_writer.close_part(false);
                open_section = OpenSection::from_continued_pane_id(pane_update.pane_id);
                if !part_writer.can_take_section_bytes(
                    open_section.compute_byte_count_bound() + instruction_bytes.len(),
                ) {
                    return Err(PaintedUpdateEncodeError::RowTooLarge {
                        row_byte_count: instruction_bytes.len() - 1,
                    });
                }
            }
            open_section
                .instruction_bytes
                .extend_from_slice(&instruction_bytes);
            open_section.instruction_count += 1;
        }
        part_writer.add_section(&open_section);
    }
    part_writer.close_part(true);
    Ok(part_writer.part_payloads)
}

/// The parts of one update being encoded: the finished payloads, and the part
/// being filled.
struct PartWriter {
    maximum_part_byte_count: usize,
    part_payloads: Vec<Vec<u8>>,
    /// The update's sequence and JSON while the first part is being filled,
    /// empty after.
    opening_bytes: Vec<u8>,
    /// The part flags the first part opens the update with.
    opening_flags: u8,
    section_bytes: Vec<u8>,
    section_count: u64,
}

impl PartWriter {
    /// Whether the part being filled has room for `section_byte_count` more
    /// bytes of sections.
    fn can_take_section_bytes(&self, section_byte_count: usize) -> bool {
        PART_FRAME_BYTE_COUNT
            + self.opening_bytes.len()
            + self.section_bytes.len()
            + section_byte_count
            <= self.maximum_part_byte_count
    }

    /// Append `open_section` whole to the part being filled.
    fn add_section(&mut self, open_section: &OpenSection) {
        self.section_bytes
            .extend_from_slice(open_section.pane_id.get_uuid().as_bytes());
        self.section_bytes
            .extend_from_slice(&open_section.header_bytes);
        write_varint(open_section.instruction_count, &mut self.section_bytes);
        self.section_bytes
            .extend_from_slice(&open_section.instruction_bytes);
        self.section_count += 1;
    }

    /// Finish the part being filled as a payload, and start an empty part
    /// that does not open the update. `is_last_part` sets the flag that ends
    /// the update.
    fn close_part(&mut self, is_last_part: bool) {
        let is_first_part = self.part_payloads.is_empty();
        let mut part_flags = 0u8;
        if is_first_part {
            part_flags |= self.opening_flags;
        }
        if is_last_part {
            part_flags |= PART_ENDS_UPDATE_FLAG;
        }
        let mut part_payload = Vec::with_capacity(
            PART_FRAME_BYTE_COUNT + self.opening_bytes.len() + self.section_bytes.len(),
        );
        part_payload.push(PAINTED_UPDATE_FRAME_KIND);
        part_payload.push(part_flags);
        part_payload.append(&mut self.opening_bytes);
        write_varint(self.section_count, &mut part_payload);
        part_payload.append(&mut self.section_bytes);
        self.section_count = 0;
        self.part_payloads.push(part_payload);
    }
}

/// One section being filled: its pane, its flags and header, and its
/// instructions so far.
struct OpenSection {
    pane_id: PaneId,
    /// The section flags, then the pane header when the section opens the
    /// pane.
    header_bytes: Vec<u8>,
    instruction_bytes: Vec<u8>,
    instruction_count: u64,
}

impl OpenSection {
    /// The section that opens `pane_update`'s pane, with no instruction yet.
    fn from_pane_update(pane_update: &PaneUpdate<'_>) -> Self {
        let pane_header = pane_update.pane_header;
        let mut section_flags = SECTION_OPENS_PANE_FLAG;
        if let Some(pane_window) = pane_header.pane_window {
            section_flags |= SECTION_HAS_WINDOW_FLAG;
            if pane_window.is_reset {
                section_flags |= SECTION_RESETS_ROWS_FLAG;
            }
        }
        let mut header_bytes = vec![section_flags];
        let cursor = pane_header.cursor;
        write_varint(u64::from(cursor.row_index), &mut header_bytes);
        write_varint(u64::from(cursor.column_index), &mut header_bytes);
        let mut cursor_flags = 0u8;
        if cursor.is_visible {
            cursor_flags |= CURSOR_VISIBLE_FLAG;
        }
        if cursor.is_blinking {
            cursor_flags |= CURSOR_BLINKING_FLAG;
        }
        header_bytes.push(cursor_flags);
        header_bytes.push(match cursor.shape {
            None => CURSOR_SHAPE_NONE_CODE,
            Some(PaneCursorShape::Block) => CURSOR_SHAPE_BLOCK_CODE,
            Some(PaneCursorShape::Underline) => CURSOR_SHAPE_UNDERLINE_CODE,
            Some(PaneCursorShape::Bar) => CURSOR_SHAPE_BAR_CODE,
        });
        write_varint(pane_header.view_top_row_index, &mut header_bytes);
        write_varint(pane_header.retained_line_count, &mut header_bytes);
        if let Some(pane_window) = pane_header.pane_window {
            write_varint(u64::from(pane_window.column_count), &mut header_bytes);
            write_varint(u64::from(pane_window.row_count), &mut header_bytes);
            write_varint(pane_window.view_row_offset, &mut header_bytes);
        }
        OpenSection {
            pane_id: pane_update.pane_id,
            header_bytes,
            instruction_bytes: Vec::new(),
            instruction_count: 0,
        }
    }

    /// The section that goes on with `pane_id`'s row instructions, with no
    /// instruction yet.
    fn from_continued_pane_id(pane_id: PaneId) -> Self {
        OpenSection {
            pane_id,
            header_bytes: vec![0],
            instruction_bytes: Vec::new(),
            instruction_count: 0,
        }
    }

    /// The bytes the section takes once added to a part, at most.
    fn compute_byte_count_bound(&self) -> usize {
        SECTION_FRAME_BYTE_COUNT + self.header_bytes.len() + self.instruction_bytes.len()
    }
}

/// Append the bytes of `row_instruction` to `instruction_bytes`.
fn encode_row_instruction(
    row_instruction: &RowInstruction<&[u8]>,
    instruction_bytes: &mut Vec<u8>,
) {
    match row_instruction {
        RowInstruction::Keep { row_count } => {
            instruction_bytes.push(KEEP_INSTRUCTION_TAG);
            write_varint(u64::from(*row_count), instruction_bytes);
        }
        RowInstruction::Copy {
            source_row_index,
            row_count,
        } => {
            instruction_bytes.push(COPY_INSTRUCTION_TAG);
            write_varint(u64::from(*source_row_index), instruction_bytes);
            write_varint(u64::from(*row_count), instruction_bytes);
        }
        RowInstruction::New { encoded_row_bytes } => {
            instruction_bytes.reserve(1 + encoded_row_bytes.len());
            instruction_bytes.push(NEW_INSTRUCTION_TAG);
            instruction_bytes.extend_from_slice(encoded_row_bytes);
        }
    }
}

/// Read one binary frame of the session plane from its payload. An image
/// chunk keeps `frame_payload` and copies no byte of it; a painted update part
/// copies the bytes of each new row out of it.
///
/// # Errors
/// [`SessionBinaryFrameDecodeError::MissingFrameKind`] for an empty payload.
/// For an image chunk, [`SessionBinaryFrameDecodeError::Truncated`] for a
/// payload shorter than its 18-byte head, and
/// [`SessionBinaryFrameDecodeError::ValueOutOfRange`] naming `last_chunk_flag`
/// for a flag byte other than `0` and `1`. For a painted update part whose
/// bytes do not read, the variant that names the fault: a field that ends
/// early, a varint too long, a value past its range, a flag this build does not
/// know, an unknown row instruction, bytes after the last section, or JSON that
/// does not decode.
pub fn decode_session_binary_frame(
    frame_payload: Vec<u8>,
) -> Result<SessionBinaryFrame, SessionBinaryFrameDecodeError> {
    let Some((&frame_kind, part_bytes)) = frame_payload.split_first() else {
        return Err(SessionBinaryFrameDecodeError::MissingFrameKind);
    };
    if frame_kind == IMAGE_CHUNK_FRAME_KIND {
        return decode_image_chunk_frame(frame_payload).map(SessionBinaryFrame::ImageChunk);
    }
    if frame_kind != PAINTED_UPDATE_FRAME_KIND {
        return Ok(SessionBinaryFrame::Unknown { frame_kind });
    }
    let mut part_reader = PartReader {
        remaining_bytes: part_bytes,
    };
    let painted_update_part = part_reader.read_painted_update_part()?;
    if !part_reader.remaining_bytes.is_empty() {
        return Err(SessionBinaryFrameDecodeError::TrailingBytes {
            trailing_byte_count: part_reader.remaining_bytes.len(),
        });
    }
    Ok(SessionBinaryFrame::PaintedUpdatePart(painted_update_part))
}

/// Decode one JSON value of a painted update's opening.
///
/// # Errors
/// [`SessionBinaryFrameDecodeError::MalformedStructure`] with the JSON
/// decoder's message when the bytes do not decode.
fn parse_structure_json<StructureValue: serde::de::DeserializeOwned>(
    structure_json_bytes: &[u8],
) -> Result<StructureValue, SessionBinaryFrameDecodeError> {
    serde_json::from_slice(structure_json_bytes).map_err(|parse_error| {
        SessionBinaryFrameDecodeError::MalformedStructure {
            error_detail: parse_error.to_string(),
        }
    })
}

/// A cursor over the bytes of one painted update part, after its kind byte.
struct PartReader<'payload> {
    remaining_bytes: &'payload [u8],
}

impl<'payload> PartReader<'payload> {
    fn read_painted_update_part(
        &mut self,
    ) -> Result<PaintedUpdatePart, SessionBinaryFrameDecodeError> {
        let part_flags = self.read_byte()?;
        let is_update_opening = part_flags & PART_OPENS_UPDATE_FLAG != 0;
        let is_placement_preview_changed = part_flags & PART_CHANGES_PLACEMENT_PREVIEW_FLAG != 0;
        if part_flags
            & !(PART_OPENS_UPDATE_FLAG
                | PART_ENDS_UPDATE_FLAG
                | PART_CHANGES_PLACEMENT_PREVIEW_FLAG)
            != 0
            || (is_placement_preview_changed && !is_update_opening)
        {
            return Err(SessionBinaryFrameDecodeError::UnknownFlags {
                field_name: "part_flags",
                flag_bits: part_flags,
            });
        }
        let update_opening = if is_update_opening {
            let update_sequence = self.read_varint()?;
            let structure_byte_count = self.read_varint()?;
            let structure_json_bytes = self.read_bytes(structure_byte_count)?;
            let painted_frame = if structure_json_bytes.is_empty() {
                None
            } else {
                Some(Box::new(parse_structure_json(structure_json_bytes)?))
            };
            let placement_preview_update = if is_placement_preview_changed {
                let placement_preview_byte_count = self.read_varint()?;
                let placement_preview_json_bytes = self.read_bytes(placement_preview_byte_count)?;
                if placement_preview_json_bytes.is_empty() {
                    PlacementPreviewUpdate::Cleared
                } else {
                    PlacementPreviewUpdate::Replaced(Box::new(parse_structure_json(
                        placement_preview_json_bytes,
                    )?))
                }
            } else {
                PlacementPreviewUpdate::Unchanged
            };
            Some(PaintedUpdateOpening {
                update_sequence,
                painted_frame,
                placement_preview_update,
            })
        } else {
            None
        };
        let section_count = self.read_varint()?;
        let mut pane_sections = Vec::new();
        for _ in 0..section_count {
            pane_sections.push(self.read_pane_section()?);
        }
        Ok(PaintedUpdatePart {
            update_opening,
            is_last_part: part_flags & PART_ENDS_UPDATE_FLAG != 0,
            pane_sections,
        })
    }

    fn read_pane_section(&mut self) -> Result<PaneSection, SessionBinaryFrameDecodeError> {
        let Some((pane_uuid_bytes, unread_bytes)) = self.remaining_bytes.split_first_chunk::<16>()
        else {
            return Err(SessionBinaryFrameDecodeError::Truncated);
        };
        self.remaining_bytes = unread_bytes;
        let pane_id = PaneId::from_uuid(Uuid::from_bytes(*pane_uuid_bytes));
        let section_flags = self.read_byte()?;
        let known_section_flags =
            SECTION_OPENS_PANE_FLAG | SECTION_HAS_WINDOW_FLAG | SECTION_RESETS_ROWS_FLAG;
        let is_pane_opening = section_flags & SECTION_OPENS_PANE_FLAG != 0;
        if section_flags & !known_section_flags != 0 || (!is_pane_opening && section_flags != 0) {
            return Err(SessionBinaryFrameDecodeError::UnknownFlags {
                field_name: "section_flags",
                flag_bits: section_flags,
            });
        }
        let pane_header = if is_pane_opening {
            Some(self.read_pane_header(section_flags)?)
        } else {
            None
        };
        let instruction_count = self.read_varint()?;
        let mut row_instructions = Vec::new();
        for _ in 0..instruction_count {
            row_instructions.push(self.read_row_instruction()?);
        }
        Ok(PaneSection {
            pane_id,
            pane_header,
            row_instructions,
        })
    }

    fn read_pane_header(
        &mut self,
        section_flags: u8,
    ) -> Result<PaneHeader, SessionBinaryFrameDecodeError> {
        let row_index = self.read_u16("cursor_row_index")?;
        let column_index = self.read_u16("cursor_column_index")?;
        let cursor_flags = self.read_byte()?;
        if cursor_flags & !(CURSOR_VISIBLE_FLAG | CURSOR_BLINKING_FLAG) != 0 {
            return Err(SessionBinaryFrameDecodeError::UnknownFlags {
                field_name: "cursor_flags",
                flag_bits: cursor_flags,
            });
        }
        let shape = match self.read_byte()? {
            CURSOR_SHAPE_BLOCK_CODE => Some(PaneCursorShape::Block),
            CURSOR_SHAPE_UNDERLINE_CODE => Some(PaneCursorShape::Underline),
            CURSOR_SHAPE_BAR_CODE => Some(PaneCursorShape::Bar),
            _ => None,
        };
        let view_top_row_index = self.read_varint()?;
        let retained_line_count = self.read_varint()?;
        let pane_window = if section_flags & SECTION_HAS_WINDOW_FLAG != 0 {
            Some(PaneWindow {
                column_count: self.read_u16("column_count")?,
                row_count: self.read_u16("row_count")?,
                view_row_offset: self.read_varint()?,
                is_reset: section_flags & SECTION_RESETS_ROWS_FLAG != 0,
            })
        } else if section_flags & SECTION_RESETS_ROWS_FLAG != 0 {
            return Err(SessionBinaryFrameDecodeError::UnknownFlags {
                field_name: "section_flags",
                flag_bits: section_flags,
            });
        } else {
            None
        };
        Ok(PaneHeader {
            cursor: PaneCursor {
                row_index,
                column_index,
                is_visible: cursor_flags & CURSOR_VISIBLE_FLAG != 0,
                is_blinking: cursor_flags & CURSOR_BLINKING_FLAG != 0,
                shape,
            },
            view_top_row_index,
            retained_line_count,
            pane_window,
        })
    }

    fn read_row_instruction(
        &mut self,
    ) -> Result<RowInstruction<Vec<u8>>, SessionBinaryFrameDecodeError> {
        match self.read_byte()? {
            KEEP_INSTRUCTION_TAG => Ok(RowInstruction::Keep {
                row_count: self.read_row_count()?,
            }),
            COPY_INSTRUCTION_TAG => Ok(RowInstruction::Copy {
                source_row_index: self.read_u16("source_row_index")?,
                row_count: self.read_row_count()?,
            }),
            NEW_INSTRUCTION_TAG => {
                let row_start_bytes = self.remaining_bytes;
                let body_byte_count = self.read_varint()?;
                self.read_bytes(body_byte_count)?;
                let row_byte_count = row_start_bytes.len() - self.remaining_bytes.len();
                Ok(RowInstruction::New {
                    encoded_row_bytes: row_start_bytes[..row_byte_count].to_vec(),
                })
            }
            instruction_tag => {
                Err(SessionBinaryFrameDecodeError::UnknownRowInstruction { instruction_tag })
            }
        }
    }

    /// A row count of a keep or copy: 1 to 65 535.
    fn read_row_count(&mut self) -> Result<u16, SessionBinaryFrameDecodeError> {
        let row_count = self.read_u16("row_count")?;
        if row_count == 0 {
            return Err(SessionBinaryFrameDecodeError::ValueOutOfRange {
                field_name: "row_count",
                decoded_value: 0,
            });
        }
        Ok(row_count)
    }

    fn read_u16(&mut self, field_name: &'static str) -> Result<u16, SessionBinaryFrameDecodeError> {
        let decoded_value = self.read_varint()?;
        u16::try_from(decoded_value).map_err(|_| SessionBinaryFrameDecodeError::ValueOutOfRange {
            field_name,
            decoded_value,
        })
    }

    fn read_varint(&mut self) -> Result<u64, SessionBinaryFrameDecodeError> {
        read_varint(&mut self.remaining_bytes).map_err(|varint_error| match varint_error {
            VarintError::Truncated => SessionBinaryFrameDecodeError::Truncated,
            VarintError::TooLong => SessionBinaryFrameDecodeError::VarintTooLong,
        })
    }

    fn read_bytes(
        &mut self,
        byte_count: u64,
    ) -> Result<&'payload [u8], SessionBinaryFrameDecodeError> {
        let byte_count =
            usize::try_from(byte_count).map_err(|_| SessionBinaryFrameDecodeError::Truncated)?;
        let (read_bytes, unread_bytes) = self
            .remaining_bytes
            .split_at_checked(byte_count)
            .ok_or(SessionBinaryFrameDecodeError::Truncated)?;
        self.remaining_bytes = unread_bytes;
        Ok(read_bytes)
    }

    fn read_byte(&mut self) -> Result<u8, SessionBinaryFrameDecodeError> {
        Ok(self.read_bytes(1)?[0])
    }
}

#[cfg(test)]
mod tests;
