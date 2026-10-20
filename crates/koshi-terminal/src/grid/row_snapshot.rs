//! One row as frames and history share it.
//!
//! A [`RowSnapshot`] holds one row in the row encoding ([`row_encoding`]), its
//! [`RowMetadata`], and the image data the encoding leaves out. It never
//! changes once built. A frame and history hold it as `Arc<RowSnapshot>`: a row
//! that does not change between two frames is one allocation in both.
//!
//! Example — the hard-ended 4-column row `ab` holds the 17 bytes
//! `10 04 00 02 01 04 00 00 00 00 01 00 02 02 61 62 00`, and
//! [`RowSnapshot::decode_row_cells`] gives back the cells `a`, `b` and two
//! default blanks.

use std::fmt::{self, Display, Formatter};
use std::sync::OnceLock;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::de::{self, Deserializer as DeserializerTrait};
use serde::{Deserialize, Serialize, Serializer};

use crate::grid::state::{Cell, CellExtra, RowEnd, RowMetadata};
use crate::row_encoding::{
    self, compute_text_byte_count, encode_padded_row_cells, DecodedRow, EncodedRowShape,
    RowDecodeError,
};
use crate::style::Style;

/// One cell of a row that carries a Kitty placeholder or a native image
/// fragment: its column and its whole continuation tail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RowImageCell {
    /// The cell's column.
    pub(crate) column_index: u16,
    /// The cell's continuation tail: its continuation code points, its Kitty
    /// placeholder and its native image fragments.
    pub(crate) cell_extra: CellExtra,
}

/// A hard-ended row at another column count: its snapshot, and the count of
/// cells its runs hold.
#[derive(Debug)]
pub(crate) struct RepaddedRow {
    /// The row at the new column count.
    pub(crate) row_snapshot: RowSnapshot,
    /// The count of cells the row's runs hold.
    pub(crate) run_cell_count: u16,
}

/// One row, read-only: its cells in the row encoding, how it ends, whether a
/// shell marked a prompt on it, and the image cells the encoding leaves out.
#[derive(Debug)]
pub struct RowSnapshot {
    /// The row in the row encoding.
    encoded_row_bytes: Vec<u8>,
    /// How the row ends, and whether a shell marked a prompt on it. The row
    /// end equals the one `encoded_row_bytes` holds.
    row_metadata: RowMetadata,
    /// The text byte count [`row_encoding::encode_row_cells`] gives for the
    /// row.
    text_byte_count: usize,
    /// The cells that carry a Kitty placeholder or a native image fragment, by
    /// rising column. Empty for a row with no image cell.
    image_cells: Box<[RowImageCell]>,
    /// The decoded cells, filled by the first
    /// [`list_row_cells`](Self::list_row_cells) and kept for the life of the
    /// snapshot.
    decoded_row_cells: OnceLock<Box<[Cell]>>,
}

/// Why a stored row does not read as a [`RowSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoredRowError {
    /// The base64 text is not valid base64.
    InvalidBase64,
    /// The encoded bytes do not decode.
    Undecodable(RowDecodeError),
    /// The row end the metadata names differs from the one the encoded bytes
    /// hold.
    RowEndMismatch,
    /// An image cell names a column past the row's end.
    ImageCellPastRow {
        /// The column the image cell names.
        column_index: u16,
    },
    /// Two image cells name columns out of rising order, or name one column
    /// twice.
    ImageCellsOutOfOrder,
    /// An image cell carries neither a Kitty placeholder nor a native image
    /// fragment.
    ImageCellWithoutImage {
        /// The column the image cell names.
        column_index: u16,
    },
}

impl Display for StoredRowError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBase64 => formatter.write_str("the stored row is not valid base64"),
            Self::Undecodable(row_decode_error) => {
                write!(
                    formatter,
                    "the stored row does not decode: {row_decode_error}"
                )
            }
            Self::RowEndMismatch => {
                formatter.write_str("the stored row's metadata names another row end")
            }
            Self::ImageCellPastRow { column_index } => write!(
                formatter,
                "the stored row's image cell at column {column_index} is past the row's end"
            ),
            Self::ImageCellsOutOfOrder => {
                formatter.write_str("the stored row's image cells are not in rising column order")
            }
            Self::ImageCellWithoutImage { column_index } => write!(
                formatter,
                "the stored row's image cell at column {column_index} carries no image"
            ),
        }
    }
}

impl std::error::Error for StoredRowError {}

impl RowSnapshot {
    /// The snapshot of `row_cells` ending as `row_metadata` says. The row is
    /// encoded into `encoded_row_bytes`, which is cleared first. The buffer
    /// keeps its capacity when that capacity is at most twice the encoded
    /// length, and shrinks to the encoded length otherwise. Example: a
    /// 4 096-byte buffer that receives the 17-byte row `ab` shrinks to 17
    /// bytes; a 34-byte buffer keeps its 34 bytes.
    ///
    /// # Panics
    /// Panics when `row_cells` holds more than 65 535 cells.
    pub(crate) fn from_row_cells(
        row_cells: &[Cell],
        row_metadata: RowMetadata,
        encoded_row_bytes: Vec<u8>,
    ) -> Self {
        let column_count =
            u16::try_from(row_cells.len()).expect("a row holds at most 65 535 cells");
        Self::from_padded_row_cells(
            row_cells,
            column_count,
            None,
            row_metadata,
            encoded_row_bytes,
        )
    }

    /// The snapshot of `row_cells` followed by default blanks up to
    /// `column_count` cells, ending as `row_metadata` says. The row is encoded
    /// into `encoded_row_bytes` as [`from_row_cells`](Self::from_row_cells)
    /// encodes it. `plain_row_style` is `Some(style)` only when every cell of
    /// `row_cells` is a narrow ASCII character or blank in `style`, with no
    /// combining mark and no image data: the row is then encoded as one run,
    /// and no cell is read for image data.
    ///
    /// # Panics
    /// Panics when `row_cells` holds more than `column_count` cells.
    pub(crate) fn from_padded_row_cells(
        row_cells: &[Cell],
        column_count: u16,
        plain_row_style: Option<Style>,
        row_metadata: RowMetadata,
        mut encoded_row_bytes: Vec<u8>,
    ) -> Self {
        let text_byte_count = encode_padded_row_cells(
            row_cells,
            column_count,
            plain_row_style,
            row_metadata.row_end,
            &mut encoded_row_bytes,
        );
        if encoded_row_bytes.capacity() > 2 * encoded_row_bytes.len() {
            encoded_row_bytes.shrink_to_fit();
        }
        let mut image_cells = Vec::new();
        let cells_with_image_data = if plain_row_style.is_some() {
            &[]
        } else {
            row_cells
        };
        for (column_index, cell) in cells_with_image_data.iter().enumerate() {
            if let Some(cell_extra) = cell.get_image_cell_extra() {
                image_cells.push(RowImageCell {
                    column_index: column_index as u16,
                    cell_extra: cell_extra.clone(),
                });
            }
        }
        RowSnapshot {
            encoded_row_bytes,
            row_metadata,
            text_byte_count,
            image_cells: image_cells.into_boxed_slice(),
            decoded_row_cells: OnceLock::new(),
        }
    }

    /// The snapshot of one encoded row that a peer sent for a window
    /// `column_count` cells wide: its row end read from the bytes, no prompt
    /// mark and no image cell. The row's width is read from its header before
    /// any cell is decoded. The decoded cells are kept, so
    /// [`list_row_cells`](Self::list_row_cells) decodes nothing.
    ///
    /// # Errors
    /// [`RowDecodeError::ValueOutOfRange`] naming `column_count` for a row of
    /// another width, and the [`RowDecodeError`] of bytes that are not one
    /// encoded row.
    pub fn from_encoded_row_bytes(
        encoded_row_bytes: Vec<u8>,
        column_count: u16,
    ) -> Result<Self, RowDecodeError> {
        let encoded_row_shape = row_encoding::read_encoded_row_shape(&encoded_row_bytes)?;
        if encoded_row_shape.column_count != column_count {
            return Err(RowDecodeError::ValueOutOfRange {
                field_name: "column_count",
                decoded_value: u64::from(encoded_row_shape.column_count),
            });
        }
        let DecodedRow { row_cells, row_end } = row_encoding::decode_row_cells(&encoded_row_bytes)?;
        let text_byte_count = compute_text_byte_count(&row_cells, row_end);
        Ok(RowSnapshot {
            encoded_row_bytes,
            row_metadata: RowMetadata {
                row_end,
                has_prompt_mark: false,
            },
            text_byte_count,
            image_cells: Box::default(),
            decoded_row_cells: OnceLock::from(row_cells.into_boxed_slice()),
        })
    }

    /// The snapshot a stored row holds: `encoded_row_bytes` with
    /// `row_metadata` and `image_cells`, encoded again as
    /// [`from_row_cells`](Self::from_row_cells) encodes a row.
    ///
    /// # Errors
    /// [`StoredRowError::Undecodable`] for bytes that do not decode,
    /// [`StoredRowError::RowEndMismatch`] when `row_metadata` names another row
    /// end than the bytes, and the image-cell variants for an image cell past
    /// the row, out of rising column order, or carrying no image.
    pub(crate) fn from_stored_row(
        encoded_row_bytes: &[u8],
        row_metadata: RowMetadata,
        image_cells: Vec<RowImageCell>,
    ) -> Result<Self, StoredRowError> {
        let DecodedRow {
            mut row_cells,
            row_end,
        } = row_encoding::decode_row_cells(encoded_row_bytes)
            .map_err(StoredRowError::Undecodable)?;
        if row_end != row_metadata.row_end {
            return Err(StoredRowError::RowEndMismatch);
        }
        let mut previous_column_index: Option<u16> = None;
        for image_cell in image_cells {
            let column_index = image_cell.column_index;
            if previous_column_index
                .is_some_and(|previous_column_index| column_index <= previous_column_index)
            {
                return Err(StoredRowError::ImageCellsOutOfOrder);
            }
            previous_column_index = Some(column_index);
            let Some(row_cell) = row_cells.get_mut(usize::from(column_index)) else {
                return Err(StoredRowError::ImageCellPastRow { column_index });
            };
            row_cell.set_cell_extra(image_cell.cell_extra);
            if row_cell.get_image_cell_extra().is_none() {
                return Err(StoredRowError::ImageCellWithoutImage { column_index });
            }
        }
        Ok(Self::from_row_cells(
            &row_cells,
            row_metadata,
            Vec::with_capacity(encoded_row_bytes.len()),
        ))
    }

    /// The row in the row encoding.
    #[must_use]
    pub fn get_encoded_row_bytes(&self) -> &[u8] {
        &self.encoded_row_bytes
    }

    /// How the row ends, and whether a shell marked a prompt on it.
    #[must_use]
    pub fn get_row_metadata(&self) -> RowMetadata {
        self.row_metadata
    }

    /// The text byte count of the row as history counts it: the UTF-8 length
    /// of each base character and its continuation code points, over every
    /// cell of width 1 or 2 that history keeps. A hard-ended row keeps no
    /// trailing default blank.
    pub(crate) fn get_text_byte_count(&self) -> usize {
        self.text_byte_count
    }

    /// The cells that carry a Kitty placeholder or a native image fragment, by
    /// rising column.
    pub(crate) fn list_image_cells(&self) -> &[RowImageCell] {
        &self.image_cells
    }

    /// Every cell of the row, decoded on each call and kept nowhere. A row
    /// built from cells gives back those cells, image data included.
    ///
    /// # Panics
    /// Never: every constructor checks that the bytes decode.
    #[must_use]
    pub fn decode_row_cells(&self) -> Vec<Cell> {
        let DecodedRow { mut row_cells, .. } =
            row_encoding::decode_row_cells(&self.encoded_row_bytes)
                .expect("a row snapshot holds bytes that decode");
        self.restore_image_cells(&mut row_cells);
        row_cells
    }

    /// Put each image cell's continuation tail back on the cell at its column
    /// of `row_cells`, which starts at the row's first column.
    ///
    /// # Panics
    /// When `row_cells` ends before an image cell's column.
    fn restore_image_cells(&self, row_cells: &mut [Cell]) {
        for image_cell in self.image_cells.iter() {
            row_cells[usize::from(image_cell.column_index)]
                .set_cell_extra(image_cell.cell_extra.clone());
        }
    }

    /// This hard-ended row at `column_count` columns: the same cells, then
    /// default blanks up to `column_count`, with the same metadata and image
    /// cells. `None` for a row that does not end hard, or whose runs hold
    /// `column_count` cells or more.
    ///
    /// # Panics
    /// Never: every constructor checks that the bytes decode.
    pub(crate) fn repad_hard_row(&self, column_count: u16) -> Option<RepaddedRow> {
        if self.row_metadata.row_end != RowEnd::Hard {
            return None;
        }
        let mut repadded_row_bytes = Vec::new();
        let run_cell_count = row_encoding::repad_encoded_row(
            &self.encoded_row_bytes,
            column_count,
            &mut repadded_row_bytes,
        )
        .expect("a row snapshot holds bytes that decode")?;
        Some(RepaddedRow {
            row_snapshot: RowSnapshot {
                encoded_row_bytes: repadded_row_bytes,
                row_metadata: self.row_metadata,
                text_byte_count: self.text_byte_count,
                image_cells: self.image_cells.clone(),
                decoded_row_cells: OnceLock::new(),
            },
            run_cell_count,
        })
    }

    /// The row's column count, row end and trailing default blank count, read
    /// from its encoding's header alone.
    ///
    /// # Panics
    /// Never: every constructor checks that the bytes decode.
    pub(crate) fn get_encoded_row_shape(&self) -> EncodedRowShape {
        row_encoding::read_encoded_row_shape(&self.encoded_row_bytes)
            .expect("a row snapshot holds bytes that decode")
    }

    /// Append the cells of the row's runs to `row_cells`, image data
    /// included: every cell of the row but its trailing default blanks.
    /// Return the row's shape.
    ///
    /// # Panics
    /// Never: every constructor checks that the bytes decode, and every image
    /// cell lies in the row's runs.
    pub(crate) fn decode_row_run_cells_into(&self, row_cells: &mut Vec<Cell>) -> EncodedRowShape {
        let row_start_cell_index = row_cells.len();
        let encoded_row_shape =
            row_encoding::decode_row_run_cells_into(&self.encoded_row_bytes, row_cells)
                .expect("a row snapshot holds bytes that decode");
        self.restore_image_cells(&mut row_cells[row_start_cell_index..]);
        encoded_row_shape
    }

    /// Every cell of the row. The first call decodes the row and keeps the
    /// cells for the life of the snapshot; each call after it decodes nothing.
    #[must_use]
    pub fn list_row_cells(&self) -> &[Cell] {
        self.decoded_row_cells
            .get_or_init(|| self.decode_row_cells().into_boxed_slice())
    }

    /// The encoded bytes, given back for the next row to encode into.
    pub(crate) fn into_encoded_row_bytes(self) -> Vec<u8> {
        self.encoded_row_bytes
    }
}

impl PartialEq for RowSnapshot {
    /// Two snapshots are equal when their encoded bytes, row metadata and image
    /// cells are equal. Whether the decoded cells are kept does not count.
    fn eq(&self, other_row_snapshot: &Self) -> bool {
        self.encoded_row_bytes == other_row_snapshot.encoded_row_bytes
            && self.row_metadata == other_row_snapshot.row_metadata
            && self.image_cells == other_row_snapshot.image_cells
    }
}

impl Eq for RowSnapshot {}

/// The stored form of a [`RowSnapshot`] in a resume file: the encoded bytes
/// as standard base64, the row metadata, and the image cells, left out when
/// there are none.
#[derive(Serialize, Deserialize)]
struct StoredRow {
    encoded_row: String,
    row_metadata: RowMetadata,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    image_cells: Vec<RowImageCell>,
}

impl Serialize for RowSnapshot {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        StoredRow {
            encoded_row: STANDARD.encode(&self.encoded_row_bytes),
            row_metadata: self.row_metadata,
            image_cells: self.image_cells.to_vec(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RowSnapshot {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        let stored_row = StoredRow::deserialize(deserializer)?;
        let encoded_row_bytes = STANDARD
            .decode(stored_row.encoded_row)
            .map_err(|_| de::Error::custom(StoredRowError::InvalidBase64))?;
        RowSnapshot::from_stored_row(
            &encoded_row_bytes,
            stored_row.row_metadata,
            stored_row.image_cells,
        )
        .map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests;
