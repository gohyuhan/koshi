//! The row encoding: one terminal row as a compact byte string. History keeps
//! rows in this form, and the wire carries the same bytes.
//!
//! [`encode_row_cells`] writes a row and [`decode_row_cells`] reads it back,
//! cell for cell. A soft-ended 200-column row of ASCII text in the default
//! style, with no character 4 times in a row, encodes to 219 bytes: the
//! header, one style entry, and one run that holds the 200 characters as text.
//!
//! # Layout
//!
//! Every integer is an unsigned LEB128 varint: seven bits per byte, low bits
//! first, the high bit set on every byte but the last. A varint takes at most
//! 10 bytes.
//!
//! ```text
//! body_byte_count        the byte count of everything after this varint
//! column_count           at most 65 535
//! row_end                0 hard, 1 soft, 2 soft wide
//! trailing_blank_count   default blanks at the row's end, held in no run
//! style_count, then each style entry:
//!     entry_byte_count   the byte count of the fields after this varint
//!     foreground color, background color, attribute word, underline color
//! run_count, then each run of cells in one style:
//!     style_index, cell_count, text_byte_count, the text as UTF-8,
//!     exception_count, then each exception:
//!         item_index_delta, exception_kind, its payload
//! ```
//!
//! A color is `0` (the default color), `1` and one palette byte, or `2` and
//! three bytes red, green, blue. An underline color is `0` for none, or `1`
//! and a color. A decoder reads the style fields it knows and skips the rest
//! of the entry, so a field that a newer build appends reads in an older one.
//!
//! The text of a run holds one base character per item. An item is one cell
//! of width 1 with no continuation code point, unless an exception says
//! otherwise. Exceptions name items in rising order, by their distance from
//! the item the previous exception named:
//!
//! - kind `0`, a wide pair: the item is a cell of width 2 and the
//!   continuation cell after it, a space of width 0 in the same style;
//! - kind `1` and a width: the item is one cell of width `0`, `1` or `2`;
//! - kind `2`, a byte count and that many bytes of UTF-8: the continuation
//!   code points of the item's base cell, at most 32;
//! - kind `3` and a count: the item appears that many more times. A run of 4 or
//!   more equal items travels once.
//!
//! One item takes at most one exception of each kind, in the order of their
//! kinds; kinds `0` and `1` exclude each other.
//!
//! Example — the hard-ended 4-column row `ab` followed by 2 default blanks:
//! `10 04 00 02 01 04 00 00 00 00 01 00 02 02 61 62 00`, which reads as a body
//! of 16 bytes, 4 columns, a hard end, 2 trailing blanks, one style entry of 4
//! bytes holding the default style, and one run of style 0 holding 2 cells as
//! the text `ab` with no exception.
//!
//! Image data in a cell — a Kitty placeholder and native image fragments — is
//! not part of the encoding, and a Kitty placeholder cell travels without its
//! continuation code points. History keeps both beside the encoded row.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};

use koshi_core::varint::{read_varint, write_varint, VarintError, MAX_VARINT_BYTE_COUNT};

use crate::grid::state::{count_row_content_cells, Cell, RowEnd, MAX_GRAPHEME_CONTINUATION_COUNT};
use crate::style::{AttributeFlags, Color, Style};

const ROW_END_HARD_CODE: u64 = 0;
const ROW_END_SOFT_CODE: u64 = 1;
const ROW_END_SOFT_WIDE_CODE: u64 = 2;

const COLOR_DEFAULT_KIND: u64 = 0;
const COLOR_INDEXED_KIND: u64 = 1;
const COLOR_RGB_KIND: u64 = 2;

const WIDE_PAIR_EXCEPTION_KIND: u64 = 0;
const WIDTH_EXCEPTION_KIND: u64 = 1;
const COMBINING_EXCEPTION_KIND: u64 = 2;
const REPEAT_EXCEPTION_KIND: u64 = 3;

/// The fewest equal items one repeat exception stands for.
const MIN_REPEATED_ITEM_COUNT: usize = 4;

/// The largest style table a row searches entry by entry. A larger table
/// finds a style through [`RowStyleTable::style_index_by_key`].
const MAX_LINEAR_STYLE_SEARCH_COUNT: usize = 32;

/// Why a byte string is not an encoded row. Every check a decoder runs has its
/// own variant; decoding never panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowDecodeError {
    /// The bytes end inside a field.
    Truncated,
    /// A varint runs past 10 bytes, or past the range of a `u64`.
    VarintTooLong,
    /// The body byte count names more bytes than follow it.
    BodyPastEnd {
        /// The byte count the row names.
        body_byte_count: u64,
        /// The byte count that follows the varint.
        available_byte_count: usize,
    },
    /// Bytes follow the body of a row read on its own.
    TrailingBytes {
        /// The count of bytes after the body.
        trailing_byte_count: usize,
    },
    /// A count is larger than its field can hold.
    ValueOutOfRange {
        /// The field that holds the count: `column_count`, `style_count`, …
        field_name: &'static str,
        /// The value read.
        decoded_value: u64,
    },
    /// The row end code is not `0`, `1` or `2`.
    UnknownRowEnd {
        /// The code read.
        row_end_code: u64,
    },
    /// A color kind is not `0`, `1` or `2`, or an underline color kind is not
    /// `0` or `1`.
    UnknownColorKind {
        /// The kind read.
        color_kind: u64,
    },
    /// A style entry's byte count names more bytes than the entry holds.
    StyleEntryPastEnd,
    /// A run names a style the table does not hold.
    StyleIndexOutOfTable {
        /// The index read.
        style_index: u64,
        /// The count of entries in the table.
        style_count: usize,
    },
    /// A run's text, or a continuation, is not valid UTF-8.
    InvalidUtf8,
    /// A width exception holds a width above 2.
    WidthTooLarge {
        /// The width read.
        width: u64,
    },
    /// An exception kind is not `0` to `3`.
    UnknownExceptionKind {
        /// The kind read.
        exception_kind: u64,
    },
    /// An exception repeats a kind its item already took, or comes after a
    /// kind that must follow it.
    ExceptionOutOfOrder,
    /// An exception names an item past the run's text.
    ExceptionPastRun,
    /// A continuation exception holds more than 32 code points.
    TooManyCombiningCharacters {
        /// The count read.
        combining_count: usize,
    },
    /// The cells of the runs and the trailing blanks do not add up to the
    /// column count, or one run's items do not add up to its cell count.
    CellCountMismatch {
        /// The column count of the row.
        column_count: u16,
        /// The count of cells the row's parts hold, up to where the check
        /// failed.
        cell_count: u64,
    },
}

impl Display for RowDecodeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => formatter.write_str("the row ends inside a field"),
            Self::VarintTooLong => {
                formatter.write_str("a varint runs past 10 bytes or past the range of a u64")
            }
            Self::BodyPastEnd {
                body_byte_count,
                available_byte_count,
            } => write!(
                formatter,
                "the row names {body_byte_count} body bytes and {available_byte_count} follow"
            ),
            Self::TrailingBytes {
                trailing_byte_count,
            } => write!(
                formatter,
                "{trailing_byte_count} bytes follow the body of the row"
            ),
            Self::ValueOutOfRange { field_name, decoded_value } => {
                write!(formatter, "{field_name} {decoded_value} is out of range")
            }
            Self::UnknownRowEnd { row_end_code } => {
                write!(formatter, "row end code {row_end_code} is unknown")
            }
            Self::UnknownColorKind { color_kind } => {
                write!(formatter, "color kind {color_kind} is unknown")
            }
            Self::StyleEntryPastEnd => {
                formatter.write_str("a style entry names more bytes than it holds")
            }
            Self::StyleIndexOutOfTable {
                style_index,
                style_count,
            } => write!(
                formatter,
                "style {style_index} is past the {style_count} entries of the table"
            ),
            Self::InvalidUtf8 => formatter.write_str("text is not valid UTF-8"),
            Self::WidthTooLarge { width } => write!(formatter, "width {width} is above 2"),
            Self::UnknownExceptionKind { exception_kind } => {
                write!(formatter, "exception kind {exception_kind} is unknown")
            }
            Self::ExceptionOutOfOrder => formatter.write_str("an exception is out of order"),
            Self::ExceptionPastRun => {
                formatter.write_str("an exception names an item past the run")
            }
            Self::TooManyCombiningCharacters { combining_count } => write!(
                formatter,
                "a cell holds {combining_count} continuation code points, above {MAX_GRAPHEME_CONTINUATION_COUNT}"
            ),
            Self::CellCountMismatch {
                column_count,
                cell_count,
            } => write!(
                formatter,
                "the row's parts hold {cell_count} cells for {column_count} columns"
            ),
        }
    }
}

impl std::error::Error for RowDecodeError {}

/// The cells and the row end one encoded row holds. `row_cells` holds exactly
/// the column count of cells: the trailing blanks are default blanks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRow {
    /// Every cell of the row, left to right.
    pub row_cells: Vec<Cell>,
    /// How the row ends.
    pub row_end: RowEnd,
}

/// The style table of the row being encoded: each distinct style once, in
/// the order the row first uses it.
#[derive(Default)]
struct RowStyleTable {
    /// The styles, by index.
    styles: Vec<Style>,
    /// [`compute_style_key`] of each style, by index.
    style_keys: Vec<u128>,
    /// The index of each style key. Empty while the table holds at most
    /// [`MAX_LINEAR_STYLE_SEARCH_COUNT`] styles; filled with every key once it
    /// holds more.
    style_index_by_key: HashMap<u128, usize>,
}

impl RowStyleTable {
    fn clear_styles(&mut self) {
        self.styles.clear();
        self.style_keys.clear();
        self.style_index_by_key.clear();
    }

    /// The index of `style` in the table. A style the table does not hold
    /// is added at the end.
    fn find_or_add_style(&mut self, style: Style) -> usize {
        let style_key = compute_style_key(style);
        let found_style_index = if self.style_keys.len() <= MAX_LINEAR_STYLE_SEARCH_COUNT {
            self.style_keys
                .iter()
                .position(|existing_style_key| *existing_style_key == style_key)
        } else {
            self.style_index_by_key.get(&style_key).copied()
        };
        if let Some(style_index) = found_style_index {
            return style_index;
        }
        let style_index = self.styles.len();
        self.styles.push(style);
        self.style_keys.push(style_key);
        if self.style_keys.len() == MAX_LINEAR_STYLE_SEARCH_COUNT + 1 {
            self.style_index_by_key
                .extend(self.style_keys.iter().enumerate().map(
                    |(existing_style_index, existing_style_key)| {
                        (*existing_style_key, existing_style_index)
                    },
                ));
        } else if self.style_keys.len() > MAX_LINEAR_STYLE_SEARCH_COUNT + 1 {
            self.style_index_by_key.insert(style_key, style_index);
        }
        style_index
    }
}

/// A number that two styles share exactly when they are equal: the
/// foreground color key in bits 0–31, the background color key in bits
/// 32–63, the attribute word in bits 64–79, and in bits 80–111 `0` for no
/// underline color or `1` plus the underline color key.
fn compute_style_key(style: Style) -> u128 {
    let underline_color_key = style
        .get_underline_color()
        .map_or(0, |underline_color| 1 + compute_color_key(underline_color));
    u128::from(compute_color_key(style.get_foreground_color()))
        | u128::from(compute_color_key(style.get_background_color())) << 32
        | u128::from(style.get_attributes().get_attribute_word()) << 64
        | u128::from(underline_color_key) << 80
}

/// A number that two colors share exactly when they are equal: `0` for the
/// default color, `0x01_00_00_nn` for palette index `nn`, and `0x02_rr_gg_bb`
/// for an RGB color.
fn compute_color_key(color: Color) -> u32 {
    match color {
        Color::Default => 0,
        Color::Indexed(palette_index) => 0x0100_0000 | u32::from(palette_index),
        Color::Rgb(red, green, blue) => {
            0x0200_0000 | u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue)
        }
    }
}

/// Work buffers one thread reuses for every row it encodes.
#[derive(Default)]
struct RowEncoderBuffers {
    row_style_table: RowStyleTable,
    head_bytes: Vec<u8>,
    run_bytes: Vec<u8>,
    run_text: Vec<u8>,
    run_exceptions: Vec<u8>,
}

thread_local! {
    static ROW_ENCODER_BUFFERS: RefCell<RowEncoderBuffers> = RefCell::new(RowEncoderBuffers::default());
}

/// Write `row_cells` with `row_end` into `encoded_row_bytes`, which is cleared
/// first, and return the row's text byte count: the UTF-8 length of each base
/// character and its continuation code points, over every cell of width 1 or 2
/// that history keeps. A hard-ended row keeps no trailing default blank; a
/// soft-ended row keeps every cell. A Kitty placeholder cell counts its base
/// character alone.
///
/// Example — the hard-ended 200-column row `README.md` returns `9`; the same
/// cells soft-ended return `200`.
///
/// # Panics
/// Panics when `row_cells` holds more than 65 535 cells.
pub fn encode_row_cells(
    row_cells: &[Cell],
    row_end: RowEnd,
    encoded_row_bytes: &mut Vec<u8>,
) -> usize {
    let column_count = u16::try_from(row_cells.len()).expect("a row holds at most 65 535 cells");
    encode_padded_row_cells(row_cells, column_count, None, row_end, encoded_row_bytes)
}

/// Write `row_cells` followed by default blanks up to `column_count` cells,
/// with `row_end`, into `encoded_row_bytes`, which is cleared first. Return
/// the text byte count [`encode_row_cells`] returns for the padded row.
///
/// `plain_row_style` is `Some(style)` only when every cell of `row_cells` is
/// a narrow ASCII character in `style` with no continuation code point: the
/// cells then form one run, and each cell's character is the only field read.
/// With `None`, every field of each cell is read. Both give the same bytes for
/// the same cells.
///
/// Example — the cells `ab` with `column_count` 4 write the bytes that
/// [`encode_row_cells`] writes for `ab` and 2 default blanks.
///
/// # Panics
/// Panics when `row_cells` holds more than `column_count` cells.
pub(crate) fn encode_padded_row_cells(
    row_cells: &[Cell],
    column_count: u16,
    plain_row_style: Option<Style>,
    row_end: RowEnd,
    encoded_row_bytes: &mut Vec<u8>,
) -> usize {
    assert!(
        row_cells.len() <= usize::from(column_count),
        "a padded row holds at most its column count of cells"
    );
    debug_assert!(
        plain_row_style.is_none_or(|plain_row_style| row_cells.iter().all(|cell| {
            cell.get_style() == plain_row_style
                && cell.get_character().is_ascii()
                && is_plain_narrow_cell(cell)
                && cell.get_image_cell_extra().is_none()
        })),
        "every cell of a plain row is a narrow ASCII cell in the row's style"
    );
    ROW_ENCODER_BUFFERS.with(|encoder_buffers| {
        let mut encoder_buffers = encoder_buffers.borrow_mut();
        encode_row_cells_with_buffers(
            row_cells,
            column_count,
            plain_row_style.is_some(),
            row_end,
            encoded_row_bytes,
            &mut encoder_buffers,
        )
    })
}

fn encode_row_cells_with_buffers(
    row_cells: &[Cell],
    column_count: u16,
    is_plain_row: bool,
    row_end: RowEnd,
    encoded_row_bytes: &mut Vec<u8>,
    encoder_buffers: &mut RowEncoderBuffers,
) -> usize {
    let content_cell_count = count_row_content_cells(row_cells);
    let trailing_blank_count = usize::from(column_count) - content_cell_count;
    let content_cells = &row_cells[..content_cell_count];

    let RowEncoderBuffers {
        row_style_table,
        head_bytes,
        run_bytes,
        run_text,
        run_exceptions,
    } = encoder_buffers;
    row_style_table.clear_styles();
    run_bytes.clear();
    let mut run_count = 0u64;
    let mut text_byte_count = if row_end == RowEnd::Hard {
        0
    } else {
        trailing_blank_count
    };
    let mut run_start_index = 0usize;
    while run_start_index < content_cells.len() {
        let following_cells = &content_cells[run_start_index..];
        let run_style = following_cells[0].get_style();
        let run_cell_count = if is_plain_row {
            following_cells.len()
        } else {
            following_cells
                .iter()
                .position(|cell| cell.get_style() != run_style)
                .unwrap_or(following_cells.len())
        };
        let style_index = row_style_table.find_or_add_style(run_style);
        text_byte_count += encode_run(
            &following_cells[..run_cell_count],
            style_index,
            is_plain_row,
            run_bytes,
            run_text,
            run_exceptions,
        );
        run_count += 1;
        run_start_index += run_cell_count;
    }

    head_bytes.clear();
    write_varint(u64::from(column_count), head_bytes);
    write_varint(compute_row_end_code(row_end), head_bytes);
    write_varint(trailing_blank_count as u64, head_bytes);
    write_varint(row_style_table.styles.len() as u64, head_bytes);
    for style in &row_style_table.styles {
        write_style_entry(*style, head_bytes);
    }
    write_varint(run_count, head_bytes);

    let body_byte_count = head_bytes.len() + run_bytes.len();
    encoded_row_bytes.clear();
    encoded_row_bytes.reserve(MAX_VARINT_BYTE_COUNT + body_byte_count);
    write_varint(body_byte_count as u64, encoded_row_bytes);
    encoded_row_bytes.extend_from_slice(head_bytes);
    encoded_row_bytes.extend_from_slice(run_bytes);
    text_byte_count
}

/// The text byte count [`encode_row_cells`] returns for `row_cells` ending
/// with `row_end`, counted without encoding the row.
///
/// Example — the hard-ended 200-column row `README.md` gives `9`; the same
/// cells soft-ended give `200`.
pub fn compute_text_byte_count(row_cells: &[Cell], row_end: RowEnd) -> usize {
    let retained_cell_count = if row_end == RowEnd::Hard {
        count_row_content_cells(row_cells)
    } else {
        row_cells.len()
    };
    row_cells[..retained_cell_count]
        .iter()
        .filter(|cell| cell.get_display_width() != 0)
        .map(|cell| {
            cell.get_character().len_utf8()
                + count_utf8_bytes(list_encoded_combining_characters(cell))
        })
        .sum()
}

/// The UTF-8 byte count of `characters`: `['e', '\u{301}']` gives `3`.
fn count_utf8_bytes(characters: &[char]) -> usize {
    characters
        .iter()
        .map(|character| character.len_utf8())
        .sum()
}

/// Append to `run_bytes` the run that holds `run_cells`, cells of one style,
/// with `style_index` as its style. Return the run's text byte count.
/// `is_plain_ascii_run` is `true` only when every cell is a narrow ASCII
/// character with no continuation code point: no cell is then checked for it.
fn encode_run(
    run_cells: &[Cell],
    style_index: usize,
    is_plain_ascii_run: bool,
    run_bytes: &mut Vec<u8>,
    run_text: &mut Vec<u8>,
    run_exceptions: &mut Vec<u8>,
) -> usize {
    run_text.clear();
    run_exceptions.clear();
    let mut exception_count = 0u64;
    let mut previous_exception_item_index = 0usize;
    let mut text_byte_count = 0usize;
    let mut item_index = 0usize;
    let mut cell_index = 0usize;
    let mut encoded_character = [0u8; 4];
    while let Some(base_cell) = run_cells.get(cell_index) {
        // The plain narrow ASCII cells from `cell_index` on. Each group of
        // `MIN_REPEATED_ITEM_COUNT` or more equal cells is one item: its
        // character once, and a repeat exception.
        let ascii_first_index = cell_index;
        let mut unwritten_cell_index = cell_index;
        let mut group_start_index = cell_index;
        let mut group_character = '\0';
        while let Some(ascii_cell) = run_cells.get(cell_index) {
            let ascii_character = ascii_cell.get_character();
            if !is_plain_ascii_run
                && (!ascii_character.is_ascii() || !is_plain_narrow_cell(ascii_cell))
            {
                break;
            }
            if ascii_character != group_character {
                if cell_index - group_start_index >= MIN_REPEATED_ITEM_COUNT {
                    item_index = write_repeated_ascii_group(
                        &run_cells[unwritten_cell_index..cell_index],
                        group_start_index - unwritten_cell_index,
                        item_index,
                        &mut previous_exception_item_index,
                        run_text,
                        run_exceptions,
                    );
                    exception_count += 1;
                    unwritten_cell_index = cell_index;
                }
                group_character = ascii_character;
                group_start_index = cell_index;
            }
            cell_index += 1;
        }
        if cell_index > ascii_first_index {
            if cell_index - group_start_index >= MIN_REPEATED_ITEM_COUNT {
                item_index = write_repeated_ascii_group(
                    &run_cells[unwritten_cell_index..cell_index],
                    group_start_index - unwritten_cell_index,
                    item_index,
                    &mut previous_exception_item_index,
                    run_text,
                    run_exceptions,
                );
                exception_count += 1;
            } else {
                push_ascii_cell_text(&run_cells[unwritten_cell_index..cell_index], run_text);
                item_index += cell_index - unwritten_cell_index;
            }
            text_byte_count += cell_index - ascii_first_index;
            continue;
        }
        let is_wide_pair = base_cell.get_display_width() == 2
            && run_cells
                .get(cell_index + 1)
                .is_some_and(is_standard_continuation_cell);
        let item_cell_count = if is_wide_pair { 2 } else { 1 };
        let item_cells = &run_cells[cell_index..cell_index + item_cell_count];
        let mut repeat_item_count = 1usize;
        while run_cells
            .get(cell_index + repeat_item_count * item_cell_count..)
            .and_then(|following_item_start_cells| {
                following_item_start_cells.get(..item_cell_count)
            })
            .is_some_and(|following_item_cells| {
                is_item_encoding_equal(item_cells, following_item_cells)
            })
        {
            repeat_item_count += 1;
        }
        if repeat_item_count < MIN_REPEATED_ITEM_COUNT {
            repeat_item_count = 1;
        }

        let base_character = base_cell.get_character();
        run_text.extend_from_slice(
            base_character
                .encode_utf8(&mut encoded_character)
                .as_bytes(),
        );
        let combining_characters = list_encoded_combining_characters(base_cell);
        let combining_byte_count = count_utf8_bytes(combining_characters);
        if base_cell.get_display_width() != 0 {
            text_byte_count +=
                (base_character.len_utf8() + combining_byte_count) * repeat_item_count;
        }

        if is_wide_pair {
            write_exception_header(
                item_index,
                &mut previous_exception_item_index,
                WIDE_PAIR_EXCEPTION_KIND,
                run_exceptions,
            );
            exception_count += 1;
        } else if base_cell.get_display_width() != 1 {
            write_exception_header(
                item_index,
                &mut previous_exception_item_index,
                WIDTH_EXCEPTION_KIND,
                run_exceptions,
            );
            write_varint(u64::from(base_cell.get_display_width()), run_exceptions);
            exception_count += 1;
        }
        if !combining_characters.is_empty() {
            write_exception_header(
                item_index,
                &mut previous_exception_item_index,
                COMBINING_EXCEPTION_KIND,
                run_exceptions,
            );
            write_varint(combining_byte_count as u64, run_exceptions);
            for combining_character in combining_characters {
                run_exceptions.extend_from_slice(
                    combining_character
                        .encode_utf8(&mut encoded_character)
                        .as_bytes(),
                );
            }
            exception_count += 1;
        }
        if repeat_item_count > 1 {
            write_exception_header(
                item_index,
                &mut previous_exception_item_index,
                REPEAT_EXCEPTION_KIND,
                run_exceptions,
            );
            write_varint((repeat_item_count - 1) as u64, run_exceptions);
            exception_count += 1;
        }

        item_index += 1;
        cell_index += item_cell_count * repeat_item_count;
    }

    write_varint(style_index as u64, run_bytes);
    write_varint(cell_index as u64, run_bytes);
    write_varint(run_text.len() as u64, run_bytes);
    run_bytes.extend_from_slice(run_text);
    write_varint(exception_count, run_bytes);
    run_bytes.extend_from_slice(run_exceptions);
    text_byte_count
}

/// Append `ascii_cells`, plain narrow ASCII cells that end in a group of
/// [`MIN_REPEATED_ITEM_COUNT`] or more equal cells starting at
/// `group_start_index`, to the run: each cell before the group as one item,
/// then the group as one item with a repeat exception. `item_index` is the
/// item index of the first cell. Return the item index after the group.
///
/// Example — `ab----` with the group at 2 appends the text `ab-`, and the
/// exception "item 2 repeats 3 more times".
fn write_repeated_ascii_group(
    ascii_cells: &[Cell],
    group_start_index: usize,
    item_index: usize,
    previous_exception_item_index: &mut usize,
    run_text: &mut Vec<u8>,
    run_exceptions: &mut Vec<u8>,
) -> usize {
    push_ascii_cell_text(&ascii_cells[..=group_start_index], run_text);
    let group_item_index = item_index + group_start_index;
    write_exception_header(
        group_item_index,
        previous_exception_item_index,
        REPEAT_EXCEPTION_KIND,
        run_exceptions,
    );
    write_varint(
        (ascii_cells.len() - group_start_index - 1) as u64,
        run_exceptions,
    );
    group_item_index + 1
}

/// Append the character of each of `ascii_cells`, cells that hold ASCII
/// characters, to `run_text` as one byte.
fn push_ascii_cell_text(ascii_cells: &[Cell], run_text: &mut Vec<u8>) {
    run_text.extend(
        ascii_cells
            .iter()
            .map(|ascii_cell| ascii_cell.get_character() as u8),
    );
}

/// Whether `cell` encodes as an item that takes no exception but a repeat: a
/// cell of width 1 whose encoding carries no continuation code point.
fn is_plain_narrow_cell(cell: &Cell) -> bool {
    cell.get_display_width() == 1 && list_encoded_combining_characters(cell).is_empty()
}

/// Whether `cell` is the right half a wide glyph leaves: a space of width 0
/// with no continuation code point. Its style is the run's style.
fn is_standard_continuation_cell(cell: &Cell) -> bool {
    cell.get_display_width() == 0
        && cell.get_character() == ' '
        && list_encoded_combining_characters(cell).is_empty()
}

/// The continuation code points the encoding carries for `cell`: none for a
/// Kitty placeholder cell, and every one for any other cell.
fn list_encoded_combining_characters(cell: &Cell) -> &[char] {
    if cell.has_image_placeholder() {
        return &[];
    }
    cell.list_combining_characters()
}

/// Whether two items of one run encode to the same item: equal characters,
/// widths and continuation code points, cell by cell.
fn is_item_encoding_equal(first_item_cells: &[Cell], second_item_cells: &[Cell]) -> bool {
    first_item_cells
        .iter()
        .zip(second_item_cells)
        .all(|(first_cell, second_cell)| {
            first_cell.get_character() == second_cell.get_character()
                && first_cell.get_display_width() == second_cell.get_display_width()
                && list_encoded_combining_characters(first_cell)
                    == list_encoded_combining_characters(second_cell)
        })
}

fn write_exception_header(
    item_index: usize,
    previous_exception_item_index: &mut usize,
    exception_kind: u64,
    run_exceptions: &mut Vec<u8>,
) {
    write_varint(
        (item_index - *previous_exception_item_index) as u64,
        run_exceptions,
    );
    write_varint(exception_kind, run_exceptions);
    *previous_exception_item_index = item_index;
}

/// Append one style entry: its byte count, then its fields. The fields take
/// at most 15 bytes, and the byte count takes one byte.
fn write_style_entry(style: Style, output_bytes: &mut Vec<u8>) {
    let entry_byte_count_index = output_bytes.len();
    output_bytes.push(0);
    write_color(style.get_foreground_color(), output_bytes);
    write_color(style.get_background_color(), output_bytes);
    write_varint(
        u64::from(style.get_attributes().get_attribute_word()),
        output_bytes,
    );
    match style.get_underline_color() {
        None => write_varint(0, output_bytes),
        Some(underline_color) => {
            write_varint(1, output_bytes);
            write_color(underline_color, output_bytes);
        }
    }
    output_bytes[entry_byte_count_index] = (output_bytes.len() - entry_byte_count_index - 1) as u8;
}

fn write_color(color: Color, output_bytes: &mut Vec<u8>) {
    match color {
        Color::Default => write_varint(COLOR_DEFAULT_KIND, output_bytes),
        Color::Indexed(palette_index) => {
            write_varint(COLOR_INDEXED_KIND, output_bytes);
            output_bytes.push(palette_index);
        }
        Color::Rgb(red, green, blue) => {
            write_varint(COLOR_RGB_KIND, output_bytes);
            output_bytes.extend_from_slice(&[red, green, blue]);
        }
    }
}

/// The byte count of `varint_value` as an unsigned LEB128 varint: `1` up to
/// `127`, `2` up to `16 383`, and so on.
fn compute_varint_byte_count(varint_value: u64) -> usize {
    let mut varint_byte_count = 1;
    let mut remaining_value = varint_value >> 7;
    while remaining_value != 0 {
        varint_byte_count += 1;
        remaining_value >>= 7;
    }
    varint_byte_count
}

/// The code that stands for `row_end` in an encoded row's header.
fn compute_row_end_code(row_end: RowEnd) -> u64 {
    match row_end {
        RowEnd::Hard => ROW_END_HARD_CODE,
        RowEnd::Soft => ROW_END_SOFT_CODE,
        RowEnd::SoftWide => ROW_END_SOFT_WIDE_CODE,
    }
}

/// A cursor over bytes being decoded.
struct ByteReader<'a> {
    remaining_bytes: &'a [u8],
}

impl<'a> ByteReader<'a> {
    #[inline]
    fn read_varint(&mut self) -> Result<u64, RowDecodeError> {
        read_varint(&mut self.remaining_bytes).map_err(|varint_error| match varint_error {
            VarintError::Truncated => RowDecodeError::Truncated,
            VarintError::TooLong => RowDecodeError::VarintTooLong,
        })
    }

    fn read_bounded_varint(
        &mut self,
        field_name: &'static str,
        maximum_value: u64,
    ) -> Result<u64, RowDecodeError> {
        let decoded_value = self.read_varint()?;
        if decoded_value > maximum_value {
            return Err(RowDecodeError::ValueOutOfRange {
                field_name,
                decoded_value,
            });
        }
        Ok(decoded_value)
    }

    /// Read the header of one encoded row: its body byte count, which is
    /// skipped, its column count, its row end and its trailing default blank
    /// count.
    fn read_row_header(&mut self) -> Result<EncodedRowShape, RowDecodeError> {
        let _body_byte_count = self.read_varint()?;
        let column_count = self.read_bounded_varint("column_count", u64::from(u16::MAX))? as u16;
        let row_end = match self.read_varint()? {
            ROW_END_HARD_CODE => RowEnd::Hard,
            ROW_END_SOFT_CODE => RowEnd::Soft,
            ROW_END_SOFT_WIDE_CODE => RowEnd::SoftWide,
            row_end_code => return Err(RowDecodeError::UnknownRowEnd { row_end_code }),
        };
        let trailing_blank_count =
            self.read_bounded_varint("trailing_blank_count", u64::from(column_count))? as u16;
        Ok(EncodedRowShape {
            column_count,
            row_end,
            trailing_blank_count,
        })
    }

    fn read_bytes(&mut self, byte_count: u64) -> Result<&'a [u8], RowDecodeError> {
        let byte_count = usize::try_from(byte_count).map_err(|_| RowDecodeError::Truncated)?;
        let (read_bytes, unread_bytes) = self
            .remaining_bytes
            .split_at_checked(byte_count)
            .ok_or(RowDecodeError::Truncated)?;
        self.remaining_bytes = unread_bytes;
        Ok(read_bytes)
    }

    fn read_byte(&mut self) -> Result<u8, RowDecodeError> {
        Ok(self.read_bytes(1)?[0])
    }

    fn read_color(&mut self) -> Result<Color, RowDecodeError> {
        match self.read_varint()? {
            COLOR_DEFAULT_KIND => Ok(Color::Default),
            COLOR_INDEXED_KIND => Ok(Color::Indexed(self.read_byte()?)),
            COLOR_RGB_KIND => {
                let color_bytes = self.read_bytes(3)?;
                Ok(Color::Rgb(color_bytes[0], color_bytes[1], color_bytes[2]))
            }
            color_kind => Err(RowDecodeError::UnknownColorKind { color_kind }),
        }
    }
}

/// Split the first encoded row off `stream_bytes`: the row's own bytes, from
/// its body byte count through its last body byte, and the bytes after it.
///
/// # Errors
/// [`RowDecodeError::Truncated`] or [`RowDecodeError::VarintTooLong`] when the
/// body byte count does not read, and [`RowDecodeError::BodyPastEnd`] when the
/// body runs past `stream_bytes`.
pub fn split_encoded_row(stream_bytes: &[u8]) -> Result<(&[u8], &[u8]), RowDecodeError> {
    let mut byte_reader = ByteReader {
        remaining_bytes: stream_bytes,
    };
    let body_byte_count = byte_reader.read_varint()?;
    let prefix_byte_count = stream_bytes.len() - byte_reader.remaining_bytes.len();
    let available_byte_count = byte_reader.remaining_bytes.len();
    let body_byte_count = usize::try_from(body_byte_count)
        .ok()
        .filter(|body_byte_count| *body_byte_count <= available_byte_count)
        .ok_or(RowDecodeError::BodyPastEnd {
            body_byte_count,
            available_byte_count,
        })?;
    Ok(stream_bytes.split_at(prefix_byte_count + body_byte_count))
}

/// Write into `repadded_row_bytes`, which is cleared first, the encoded row in
/// `encoded_row_bytes` at `column_count` columns: the same row end, style
/// table and runs, then default blanks up to `column_count`. Return the count
/// of cells the runs hold, or `None` with nothing written when the runs hold
/// `column_count` cells or more.
///
/// Example — the hard-ended 4-column row `ab` at 6 columns writes
/// `10 06 00 04 01 04 00 00 00 00 01 00 02 02 61 62 00` and returns
/// `Some(2)`.
///
/// # Errors
/// [`RowDecodeError::Truncated`], [`RowDecodeError::VarintTooLong`],
/// [`RowDecodeError::ValueOutOfRange`] or [`RowDecodeError::UnknownRowEnd`]
/// when the header does not read.
pub(crate) fn repad_encoded_row(
    encoded_row_bytes: &[u8],
    column_count: u16,
    repadded_row_bytes: &mut Vec<u8>,
) -> Result<Option<u16>, RowDecodeError> {
    let mut byte_reader = ByteReader {
        remaining_bytes: encoded_row_bytes,
    };
    let EncodedRowShape {
        column_count: encoded_column_count,
        row_end,
        trailing_blank_count,
    } = byte_reader.read_row_header()?;
    let row_end_code = compute_row_end_code(row_end);
    let run_cell_count = encoded_column_count - trailing_blank_count;
    if run_cell_count >= column_count {
        return Ok(None);
    }
    let style_and_run_bytes = byte_reader.remaining_bytes;
    let repadded_trailing_blank_count = column_count - run_cell_count;
    let body_byte_count = compute_varint_byte_count(u64::from(column_count))
        + compute_varint_byte_count(row_end_code)
        + compute_varint_byte_count(u64::from(repadded_trailing_blank_count))
        + style_and_run_bytes.len();
    repadded_row_bytes.clear();
    repadded_row_bytes.reserve(MAX_VARINT_BYTE_COUNT + body_byte_count);
    write_varint(body_byte_count as u64, repadded_row_bytes);
    write_varint(u64::from(column_count), repadded_row_bytes);
    write_varint(row_end_code, repadded_row_bytes);
    write_varint(u64::from(repadded_trailing_blank_count), repadded_row_bytes);
    repadded_row_bytes.extend_from_slice(style_and_run_bytes);
    Ok(Some(run_cell_count))
}

/// Read the encoded row that `encoded_row_bytes` holds, and nothing else.
///
/// # Errors
/// The first check the bytes fail, as its [`RowDecodeError`] variant. Bytes
/// after the body are [`RowDecodeError::TrailingBytes`].
pub fn decode_row_cells(encoded_row_bytes: &[u8]) -> Result<DecodedRow, RowDecodeError> {
    let mut row_cells = Vec::new();
    let encoded_row_shape = decode_row_run_cells_into(encoded_row_bytes, &mut row_cells)?;
    row_cells.resize_with(
        usize::from(encoded_row_shape.column_count),
        Cell::build_blank,
    );
    Ok(DecodedRow {
        row_cells,
        row_end: encoded_row_shape.row_end,
    })
}

/// The column count, the row end and the trailing default blank count of the
/// encoded row that `encoded_row_bytes` starts with, read from its header
/// alone.
///
/// Example — the hard-ended 4-column row `ab` gives 4 columns, a hard end and
/// 2 trailing blanks.
///
/// # Errors
/// [`RowDecodeError::Truncated`], [`RowDecodeError::VarintTooLong`],
/// [`RowDecodeError::ValueOutOfRange`] or [`RowDecodeError::UnknownRowEnd`]
/// when the header does not read.
pub(crate) fn read_encoded_row_shape(
    encoded_row_bytes: &[u8],
) -> Result<EncodedRowShape, RowDecodeError> {
    ByteReader {
        remaining_bytes: encoded_row_bytes,
    }
    .read_row_header()
}

/// The column count, the row end and the trailing default blank count of one
/// encoded row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncodedRowShape {
    /// The row's column count.
    pub(crate) column_count: u16,
    /// How the row ends.
    pub(crate) row_end: RowEnd,
    /// The default blanks at the row's end that no run holds.
    pub(crate) trailing_blank_count: u16,
}

thread_local! {
    static ROW_DECODER_STYLES: RefCell<Vec<Style>> = const { RefCell::new(Vec::new()) };
}

/// Append the cells that the runs of the encoded row in `encoded_row_bytes`
/// hold to `row_cells`: every cell of the row but its trailing default
/// blanks. Return the row's shape.
///
/// Example — the hard-ended 4-column row `ab` with 2 trailing blanks appends
/// the cells `a` and `b`, and returns 4 columns, a hard end and 2 trailing
/// blanks.
///
/// # Errors
/// The errors of [`decode_row_cells`]. On an error, `row_cells` holds the
/// cells it held before the call, and no other.
pub(crate) fn decode_row_run_cells_into(
    encoded_row_bytes: &[u8],
    row_cells: &mut Vec<Cell>,
) -> Result<EncodedRowShape, RowDecodeError> {
    let row_start_cell_index = row_cells.len();
    let decode_result = ROW_DECODER_STYLES.with(|row_styles| {
        decode_row_runs(
            encoded_row_bytes,
            &mut row_styles.borrow_mut(),
            row_start_cell_index,
            row_cells,
        )
    });
    if decode_result.is_err() {
        row_cells.truncate(row_start_cell_index);
    }
    decode_result
}

/// Read the encoded row in `encoded_row_bytes` with `row_styles` as the style
/// table buffer, and append its run cells to `row_cells`, whose cells from
/// `row_start_cell_index` on are this row's.
fn decode_row_runs(
    encoded_row_bytes: &[u8],
    row_styles: &mut Vec<Style>,
    row_start_cell_index: usize,
    row_cells: &mut Vec<Cell>,
) -> Result<EncodedRowShape, RowDecodeError> {
    let (encoded_row, trailing_bytes) = split_encoded_row(encoded_row_bytes)?;
    if !trailing_bytes.is_empty() {
        return Err(RowDecodeError::TrailingBytes {
            trailing_byte_count: trailing_bytes.len(),
        });
    }
    let mut byte_reader = ByteReader {
        remaining_bytes: encoded_row,
    };
    let EncodedRowShape {
        column_count,
        row_end,
        trailing_blank_count,
    } = byte_reader.read_row_header()?;

    let style_count = byte_reader
        .read_bounded_varint("style_count", byte_reader.remaining_bytes.len() as u64)?
        as usize;
    row_styles.clear();
    for _ in 0..style_count {
        let style_entry_byte_count = byte_reader.read_varint()?;
        let style_entry_bytes = byte_reader
            .read_bytes(style_entry_byte_count)
            .map_err(|_| RowDecodeError::StyleEntryPastEnd)?;
        row_styles.push(read_style_fields(style_entry_bytes)?);
    }

    let content_cell_count = u64::from(column_count - trailing_blank_count);
    let run_count =
        byte_reader.read_bounded_varint("run_count", byte_reader.remaining_bytes.len() as u64)?;
    row_cells.reserve(usize::from(column_count));
    for _ in 0..run_count {
        decode_run(
            &mut byte_reader,
            row_styles,
            column_count,
            content_cell_count,
            row_start_cell_index,
            row_cells,
        )?;
    }
    let decoded_cell_count = (row_cells.len() - row_start_cell_index) as u64;
    if decoded_cell_count != content_cell_count {
        return Err(RowDecodeError::CellCountMismatch {
            column_count,
            cell_count: decoded_cell_count + u64::from(trailing_blank_count),
        });
    }
    if !byte_reader.remaining_bytes.is_empty() {
        return Err(RowDecodeError::TrailingBytes {
            trailing_byte_count: byte_reader.remaining_bytes.len(),
        });
    }
    Ok(EncodedRowShape {
        column_count,
        row_end,
        trailing_blank_count,
    })
}

fn read_style_fields(style_entry_bytes: &[u8]) -> Result<Style, RowDecodeError> {
    let mut entry_reader = ByteReader {
        remaining_bytes: style_entry_bytes,
    };
    let foreground_color = entry_reader.read_color()?;
    let background_color = entry_reader.read_color()?;
    let attribute_word = entry_reader.read_varint()?;
    let underline_color = match entry_reader.read_varint()? {
        0 => None,
        1 => Some(entry_reader.read_color()?),
        color_kind => return Err(RowDecodeError::UnknownColorKind { color_kind }),
    };
    Ok(Style::from_style_parts(
        foreground_color,
        background_color,
        AttributeFlags::from_attribute_word(attribute_word as u16),
        underline_color,
    ))
}

/// The shape one item takes, from its exceptions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemShape {
    Single { display_width: u8 },
    WidePair,
}

/// Read one run and append its cells to `row_cells`, whose cells from
/// `row_start_cell_index` on are the row's. The items before each exception
/// are cells of width 1; a stretch of them written in ASCII is appended in one
/// step.
fn decode_run(
    byte_reader: &mut ByteReader<'_>,
    styles: &[Style],
    column_count: u16,
    content_cell_count: u64,
    row_start_cell_index: usize,
    row_cells: &mut Vec<Cell>,
) -> Result<(), RowDecodeError> {
    let style_index = byte_reader.read_varint()?;
    let run_style = *usize::try_from(style_index)
        .ok()
        .and_then(|style_index| styles.get(style_index))
        .ok_or(RowDecodeError::StyleIndexOutOfTable {
            style_index,
            style_count: styles.len(),
        })?;
    let run_cell_count = byte_reader.read_varint()?;
    let mut decoded_cell_count = (row_cells.len() - row_start_cell_index) as u64;
    let run_end_cell_count = decoded_cell_count
        .checked_add(run_cell_count)
        .filter(|run_end_cell_count| *run_end_cell_count <= content_cell_count)
        .ok_or(RowDecodeError::CellCountMismatch {
            column_count,
            cell_count: decoded_cell_count.saturating_add(run_cell_count),
        })?;
    let text_byte_count = byte_reader.read_varint()?;
    let run_text_bytes = byte_reader.read_bytes(text_byte_count)?;
    let run_text = if run_text_bytes.is_ascii() {
        RunText::Ascii(run_text_bytes)
    } else {
        RunText::Utf8(std::str::from_utf8(run_text_bytes).map_err(|_| RowDecodeError::InvalidUtf8)?)
    };
    let exception_count = byte_reader
        .read_bounded_varint("exception_count", byte_reader.remaining_bytes.len() as u64)?;

    let mut remaining_exception_count = exception_count;
    let mut next_exception = read_next_exception(byte_reader, &mut remaining_exception_count, 0)?;
    let mut combining_characters: Vec<char> = Vec::new();
    let mut text_byte_index = 0usize;
    let mut item_index = 0u64;
    while let Some(base_character) = run_text.read_character_at(text_byte_index) {
        let next_exception_item_index = next_exception
            .as_ref()
            .map_or(u64::MAX, |item_exception| item_exception.item_index);
        if item_index < next_exception_item_index {
            let plain_item_count =
                usize::try_from(next_exception_item_index - item_index).unwrap_or(usize::MAX);
            let remaining_text_bytes = &run_text_bytes[text_byte_index..];
            let plain_text_bytes =
                &remaining_text_bytes[..plain_item_count.min(remaining_text_bytes.len())];
            let ascii_item_count = if plain_text_bytes.is_ascii() {
                plain_text_bytes.len()
            } else {
                plain_text_bytes
                    .iter()
                    .take_while(|text_byte| text_byte.is_ascii())
                    .count()
            };
            if ascii_item_count > 0 {
                if decoded_cell_count + ascii_item_count as u64 > run_end_cell_count {
                    return Err(RowDecodeError::CellCountMismatch {
                        column_count,
                        cell_count: run_end_cell_count + 1,
                    });
                }
                row_cells.extend(plain_text_bytes[..ascii_item_count].iter().map(
                    move |text_byte| Cell::from_character(char::from(*text_byte), 1, run_style),
                ));
                decoded_cell_count += ascii_item_count as u64;
                item_index += ascii_item_count as u64;
                text_byte_index += ascii_item_count;
                continue;
            }
        }
        text_byte_index += base_character.len_utf8();
        if item_index < next_exception_item_index {
            if decoded_cell_count >= run_end_cell_count {
                return Err(RowDecodeError::CellCountMismatch {
                    column_count,
                    cell_count: decoded_cell_count + 1,
                });
            }
            row_cells.push(Cell::from_character(base_character, 1, run_style));
            decoded_cell_count += 1;
            item_index += 1;
            continue;
        }
        let mut item_shape = ItemShape::Single { display_width: 1 };
        combining_characters.clear();
        let mut repeat_item_count = 1u64;
        let mut last_exception_kind: Option<u64> = None;
        while let Some(item_exception) = next_exception
            .as_ref()
            .filter(|item_exception| item_exception.item_index == item_index)
        {
            let is_in_order = match (last_exception_kind, item_exception.exception_kind) {
                (None, _) => true,
                (Some(WIDE_PAIR_EXCEPTION_KIND | WIDTH_EXCEPTION_KIND), exception_kind) => {
                    exception_kind > WIDTH_EXCEPTION_KIND
                }
                (Some(previous_kind), exception_kind) => exception_kind > previous_kind,
            };
            if !is_in_order {
                return Err(RowDecodeError::ExceptionOutOfOrder);
            }
            last_exception_kind = Some(item_exception.exception_kind);
            match item_exception.payload {
                ExceptionPayload::WidePair => item_shape = ItemShape::WidePair,
                ExceptionPayload::Width(display_width) => {
                    item_shape = ItemShape::Single { display_width }
                }
                ExceptionPayload::Combining(combining_text) => {
                    combining_characters.extend(combining_text.chars());
                    if combining_characters.len() > MAX_GRAPHEME_CONTINUATION_COUNT {
                        return Err(RowDecodeError::TooManyCombiningCharacters {
                            combining_count: combining_characters.len(),
                        });
                    }
                }
                ExceptionPayload::Repeat(extra_item_count) => {
                    repeat_item_count = extra_item_count.saturating_add(1)
                }
            }
            let current_item_index = item_exception.item_index;
            next_exception = read_next_exception(
                byte_reader,
                &mut remaining_exception_count,
                current_item_index,
            )?;
        }
        let item_cell_count: u64 = match item_shape {
            ItemShape::WidePair => 2,
            ItemShape::Single { .. } => 1,
        };
        let item_end_cell_count = item_cell_count
            .checked_mul(repeat_item_count)
            .and_then(|item_cells| item_cells.checked_add(decoded_cell_count))
            .filter(|item_end_cell_count| *item_end_cell_count <= run_end_cell_count)
            .ok_or(RowDecodeError::CellCountMismatch {
                column_count,
                cell_count: decoded_cell_count
                    .saturating_add(item_cell_count.saturating_mul(repeat_item_count)),
            })?;
        let base_display_width = match item_shape {
            ItemShape::WidePair => 2,
            ItemShape::Single { display_width } => display_width,
        };
        let mut base_cell = Cell::from_character(base_character, base_display_width, run_style);
        for combining_character in &combining_characters {
            base_cell.push_combining(*combining_character);
        }
        for _ in 0..repeat_item_count {
            row_cells.push(base_cell.clone());
            if item_shape == ItemShape::WidePair {
                row_cells.push(Cell::from_character(' ', 0, run_style));
            }
        }
        decoded_cell_count = item_end_cell_count;
        item_index += 1;
    }
    if next_exception.is_some() {
        return Err(RowDecodeError::ExceptionPastRun);
    }
    if decoded_cell_count != run_end_cell_count {
        return Err(RowDecodeError::CellCountMismatch {
            column_count,
            cell_count: decoded_cell_count,
        });
    }
    Ok(())
}

/// The text of one run: ASCII bytes, or UTF-8 text the decoder checked.
enum RunText<'a> {
    Ascii(&'a [u8]),
    Utf8(&'a str),
}

impl RunText<'_> {
    /// The character that starts at `text_byte_index`, a character boundary,
    /// or `None` at the end of the text.
    fn read_character_at(&self, text_byte_index: usize) -> Option<char> {
        match self {
            RunText::Ascii(run_text_bytes) => run_text_bytes
                .get(text_byte_index)
                .map(|text_byte| char::from(*text_byte)),
            RunText::Utf8(run_text) => run_text[text_byte_index..].chars().next(),
        }
    }
}

/// What one exception says about its item, as its kind and payload read.
enum ExceptionPayload<'a> {
    WidePair,
    Width(u8),
    Combining(&'a str),
    Repeat(u64),
}

/// One exception of a run: the item it names, its kind, and its payload.
struct ItemException<'a> {
    item_index: u64,
    exception_kind: u64,
    payload: ExceptionPayload<'a>,
}

/// Read the next exception of a run, or `None` once the run's count is read.
fn read_next_exception<'a>(
    byte_reader: &mut ByteReader<'a>,
    remaining_exception_count: &mut u64,
    previous_item_index: u64,
) -> Result<Option<ItemException<'a>>, RowDecodeError> {
    if *remaining_exception_count == 0 {
        return Ok(None);
    }
    *remaining_exception_count -= 1;
    let item_index = previous_item_index
        .checked_add(byte_reader.read_varint()?)
        .ok_or(RowDecodeError::ExceptionPastRun)?;
    let exception_kind = byte_reader.read_varint()?;
    let payload = match exception_kind {
        WIDE_PAIR_EXCEPTION_KIND => ExceptionPayload::WidePair,
        WIDTH_EXCEPTION_KIND => {
            let display_width = byte_reader.read_varint()?;
            if display_width > 2 {
                return Err(RowDecodeError::WidthTooLarge {
                    width: display_width,
                });
            }
            ExceptionPayload::Width(display_width as u8)
        }
        COMBINING_EXCEPTION_KIND => {
            let combining_byte_count = byte_reader.read_varint()?;
            let combining_text = std::str::from_utf8(byte_reader.read_bytes(combining_byte_count)?)
                .map_err(|_| RowDecodeError::InvalidUtf8)?;
            ExceptionPayload::Combining(combining_text)
        }
        REPEAT_EXCEPTION_KIND => ExceptionPayload::Repeat(byte_reader.read_varint()?),
        exception_kind => return Err(RowDecodeError::UnknownExceptionKind { exception_kind }),
    };
    Ok(Some(ItemException {
        item_index,
        exception_kind,
        payload,
    }))
}

#[cfg(test)]
mod tests;
