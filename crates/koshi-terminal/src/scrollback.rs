//! Per-pane scrollback history: a bounded buffer of lines that have scrolled
//! off the top of the primary screen.
//!
//! The buffer is capped on two axes: a maximum row count and a maximum byte
//! count. When a push exceeds either cap the oldest rows are dropped from the
//! front. The count and byte size of everything dropped are tallied, never the
//! content itself. A snapshot reads the row tally as one boolean:
//! `dropped_line_count() > 0` becomes `ScrollbackMeta::truncated`.

use std::collections::VecDeque;

use serde::de::Deserializer as DeserializerTrait;
use serde::{Deserialize, Serialize};

use crate::grid::state::{count_row_content_cells, Cell, RowEnd, RowMetadata};

/// Default scrollback line cap: 10 000 lines per pane.
const DEFAULT_MAX_LINE_COUNT: usize = 10_000;
/// Default scrollback byte cap: 32 MiB of retained text per pane.
const DEFAULT_MAX_BYTE_COUNT: usize = 32 * 1024 * 1024;

/// The cells history keeps of `row`: a [`RowEnd::Hard`] row without the
/// trailing run of fully-default blanks, every other row whole.
///
/// A 200-column hard row reading `README.md` keeps 9 cells. A styled blank —
/// a background-colored prompt segment — is not a default blank and is kept.
/// A [`RowEnd::Soft`] row keeps every cell. A [`RowEnd::SoftWide`] row keeps
/// every cell, including the final blank spacer that stands in for the wide
/// glyph on the next row.
fn get_retained_cells(row_cells: &[Cell], row_end: RowEnd) -> &[Cell] {
    if row_end == RowEnd::Hard {
        &row_cells[..count_row_content_cells(row_cells)]
    } else {
        row_cells
    }
}

/// The byte size of one row: the UTF-8 length of every cell's base character
/// plus its combining marks, summed over the cells whose width is not `0`. The
/// byte cap is measured in this unit.
///
/// A width-0 cell is the placeholder right half of a wide glyph and adds
/// nothing; the glyph's text is counted in its width-2 base cell.
fn compute_line_byte_count(line_cells: &[Cell]) -> usize {
    line_cells
        .iter()
        .filter(|cell| cell.get_display_width() != 0)
        .map(|cell| {
            cell.get_character().len_utf8()
                + cell
                    .list_combining_characters()
                    .iter()
                    .map(|combining| combining.len_utf8())
                    .sum::<usize>()
        })
        .sum()
}

/// Truncate owned `row_cells` to what [`get_retained_cells`] keeps of it and
/// release the spare capacity.
fn truncate_line_cells(row_cells: &mut Vec<Cell>, row_end: RowEnd) {
    row_cells.truncate(get_retained_cells(row_cells, row_end).len());
    row_cells.shrink_to_fit();
}

/// The line- and byte-count caps bounding one pane's [`Scrollback`].
#[derive(Debug, Clone, Copy)]
pub struct ScrollbackLimit {
    maximum_line_count: usize,
    maximum_byte_count: usize,
}

impl ScrollbackLimit {
    /// A cap of exactly `maximum_line_count` rows and `maximum_byte_count` bytes.
    pub fn from_line_and_byte_limits(maximum_line_count: usize, maximum_byte_count: usize) -> Self {
        ScrollbackLimit {
            maximum_line_count,
            maximum_byte_count,
        }
    }
}

impl Default for ScrollbackLimit {
    /// 10 000 lines and 32 MiB of retained text.
    fn default() -> Self {
        ScrollbackLimit {
            maximum_line_count: DEFAULT_MAX_LINE_COUNT,
            maximum_byte_count: DEFAULT_MAX_BYTE_COUNT,
        }
    }
}

/// The scrollback buffer for one pane: a `VecDeque` of rows (oldest at the
/// front), bounded by line- and byte-count caps with truncation accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scrollback {
    /// Retained rows, oldest at the front and newest at the back, each paired
    /// with its row metadata. A row holds what [`get_retained_cells`] keeps of it: a
    /// hard-ended row stops at its last content cell and reads as blank right
    /// of that.
    retained_lines: VecDeque<(Vec<Cell>, RowMetadata)>,
    /// Maximum rows retained before the oldest are dropped.
    maximum_line_count: usize,
    /// Maximum total bytes (UTF-8 text payload) retained before the oldest rows
    /// are dropped.
    maximum_byte_count: usize,
    /// The sum of [`compute_line_byte_count`] over every retained row, updated on every
    /// push, replacement, eviction and clear.
    retained_byte_count: usize,
    /// Count of rows ever pushed into the buffer. It only grows:
    /// [`clear`](Self::clear) does not reset it.
    total_pushed_line_count: u64,
    /// Count of rows dropped to honor the caps. It only grows.
    dropped_line_count: u64,
    /// Bytes dropped to honor the caps. It only grows.
    dropped_byte_count: u64,
}

impl Scrollback {
    /// An empty buffer bounded by `limit`.
    pub fn from_scrollback_limit(scrollback_limit: ScrollbackLimit) -> Self {
        Scrollback {
            retained_lines: VecDeque::new(),
            maximum_line_count: scrollback_limit.maximum_line_count,
            maximum_byte_count: scrollback_limit.maximum_byte_count,
            retained_byte_count: 0,
            total_pushed_line_count: 0,
            dropped_line_count: 0,
            dropped_byte_count: 0,
        }
    }

    /// Append `row` as the newest line with `row_metadata`, then drop the oldest rows
    /// from the front until both caps hold, tallying each drop. The byte cap
    /// never drops the sole remaining row: a single row larger than
    /// `maximum_byte_count` is retained on arrival. The line cap has no such guard; the
    /// row count always ends at or under `maximum_line_count`.
    ///
    /// A hard-ended row is stored without its trailing run of fully-default
    /// blanks: a 200-column row reading `README.md` keeps 9 cells. A
    /// soft-wrapped row keeps every cell. One allocation, at the stored size.
    pub(crate) fn push_row(&mut self, row_cells: &[Cell], row_metadata: RowMetadata) {
        self.push_row_with_evicted(row_cells, row_metadata, |_| {});
    }

    pub(crate) fn push_row_with_evicted(
        &mut self,
        row_cells: &[Cell],
        row_metadata: RowMetadata,
        mut eviction_callback: impl FnMut(&[Cell]),
    ) {
        let retained_cells = get_retained_cells(row_cells, row_metadata.row_end).to_vec();
        let new_line_byte_count = compute_line_byte_count(&retained_cells);
        self.retained_lines
            .push_back((retained_cells, row_metadata));
        self.retained_byte_count += new_line_byte_count;
        self.total_pushed_line_count += 1;
        self.evict_oldest_lines_to_limits(&mut eviction_callback);
    }

    /// Remove and return every retained row with its metadata, oldest at the
    /// front, leaving the buffer empty with a zero byte total. The caps, the
    /// dropped tallies, and [`total_pushed_line_count`](Self::total_pushed_line_count) keep their
    /// values. The caller passes the returned rows' count to
    /// [`replace_retained_lines`](Self::replace_retained_lines) as `retained_line_count_before`.
    pub(crate) fn take_retained_lines(&mut self) -> VecDeque<(Vec<Cell>, RowMetadata)> {
        self.retained_byte_count = 0;
        std::mem::take(&mut self.retained_lines)
    }

    /// Replace every retained row with `lines`, each keeping its own metadata,
    /// then apply both caps. Rows the caps evict are tallied as dropped.
    /// [`total_pushed_line_count`](Self::total_pushed_line_count) grows by the count of retained
    /// rows (counted after eviction) exceeding `retained_line_count_before` and never
    /// decreases.
    ///
    /// Each row is stored the way [`push_row`](Self::push_row) stores one,
    /// shortened in place: a hard-ended row without its trailing default
    /// blanks, a soft-wrapped row whole.
    pub(crate) fn replace_retained_lines(
        &mut self,
        retained_lines: Vec<(Vec<Cell>, RowMetadata)>,
        retained_line_count_before: u64,
    ) {
        self.replace_retained_lines_with_evicted(
            retained_lines,
            retained_line_count_before,
            |_| {},
        );
    }

    pub(crate) fn replace_retained_lines_with_evicted(
        &mut self,
        retained_lines: Vec<(Vec<Cell>, RowMetadata)>,
        retained_line_count_before: u64,
        mut eviction_callback: impl FnMut(&[Cell]),
    ) {
        self.retained_lines = retained_lines
            .into_iter()
            .map(|(mut line_cells, row_metadata)| {
                truncate_line_cells(&mut line_cells, row_metadata.row_end);
                (line_cells, row_metadata)
            })
            .collect();
        self.retained_byte_count = self
            .retained_lines
            .iter()
            .map(|(line_cells, _)| compute_line_byte_count(line_cells))
            .sum();
        self.evict_oldest_lines_to_limits(&mut eviction_callback);
        let retained_line_count_after = self.retained_lines.len() as u64;
        self.total_pushed_line_count +=
            retained_line_count_after.saturating_sub(retained_line_count_before);
    }

    /// Drop the oldest row, update `retained_byte_count` and the dropped tallies, and
    /// repeat while the row count exceeds `maximum_line_count`, or while `retained_byte_count`
    /// exceeds `maximum_byte_count` and more than one row remains.
    fn evict_oldest_lines_to_limits(&mut self, eviction_callback: &mut impl FnMut(&[Cell])) {
        while self.retained_lines.len() > self.maximum_line_count
            || (self.retained_byte_count > self.maximum_byte_count && self.retained_lines.len() > 1)
        {
            let (oldest_line_cells, _) = self.retained_lines.pop_front().unwrap();
            let oldest_line_byte_count = compute_line_byte_count(&oldest_line_cells);
            eviction_callback(&oldest_line_cells);

            self.dropped_line_count += 1;
            self.dropped_byte_count += oldest_line_byte_count as u64;
            self.retained_byte_count -= oldest_line_byte_count;
        }
    }

    /// Drop every retained row (xterm `CSI 3 J`, "erase saved lines") and zero
    /// `retained_byte_count`. The dropped tallies and
    /// [`get_total_pushed_line_count`](Self::get_total_pushed_line_count) keep their values.
    pub fn clear_scrollback(&mut self) {
        self.retained_lines.clear();
        self.retained_byte_count = 0;
    }

    /// The number of rows currently retained.
    pub fn get_retained_line_count(&self) -> usize {
        self.retained_lines.len()
    }

    /// Whether the buffer retains no rows.
    pub fn is_empty(&self) -> bool {
        self.retained_lines.is_empty()
    }

    /// The retained rows with their metadata, oldest at the front.
    pub fn list_retained_lines(&self) -> &VecDeque<(Vec<Cell>, RowMetadata)> {
        &self.retained_lines
    }

    /// Count of rows ever pushed into the buffer. It never decreases;
    /// [`clear_scrollback`](Self::clear_scrollback) does not reset it.
    pub fn get_total_pushed_line_count(&self) -> u64 {
        self.total_pushed_line_count
    }

    /// Count of rows dropped to honor the caps.
    pub fn get_dropped_line_count(&self) -> u64 {
        self.dropped_line_count
    }

    /// Bytes dropped to honor the caps.
    pub fn get_dropped_byte_count(&self) -> u64 {
        self.dropped_byte_count
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SerializedLine {
    CurrentRow((Vec<Cell>, RowMetadata)),
    LegacyRow((Vec<Cell>, RowEnd)),
}

/// The stored form of a [`Scrollback`], as [`Deserialize`] reads it.
///
/// `retained_byte_count` is not read: it is derived from `retained_lines` instead, so a stored
/// total that does not match the rows cannot underflow the first eviction.
/// The caps are applied to the rows that were read, so a stored buffer holding
/// more than `maximum_line_count` rows loses its oldest ones at load and tallies them
/// as dropped.
#[derive(Deserialize)]
struct ScrollbackFields {
    retained_lines: VecDeque<SerializedLine>,
    maximum_line_count: usize,
    maximum_byte_count: usize,
    total_pushed_line_count: u64,
    dropped_line_count: u64,
    dropped_byte_count: u64,
}

impl<'de> Deserialize<'de> for Scrollback {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        let serialized_fields = ScrollbackFields::deserialize(deserializer)?;
        let retained_lines = serialized_fields
            .retained_lines
            .into_iter()
            .map(|serialized_line| match serialized_line {
                SerializedLine::CurrentRow((line_cells, row_metadata)) => {
                    (line_cells, row_metadata)
                }
                SerializedLine::LegacyRow((line_cells, row_end)) => (
                    line_cells,
                    RowMetadata {
                        row_end,
                        has_prompt_mark: false,
                    },
                ),
            })
            .collect();
        let mut scrollback = Scrollback {
            retained_lines,
            maximum_line_count: serialized_fields.maximum_line_count,
            maximum_byte_count: serialized_fields.maximum_byte_count,
            retained_byte_count: 0,
            total_pushed_line_count: serialized_fields.total_pushed_line_count,
            dropped_line_count: serialized_fields.dropped_line_count,
            dropped_byte_count: serialized_fields.dropped_byte_count,
        };
        scrollback.retained_byte_count = scrollback
            .retained_lines
            .iter()
            .map(|(line_cells, _)| compute_line_byte_count(line_cells))
            .sum();
        scrollback.evict_oldest_lines_to_limits(&mut |_| {});
        Ok(scrollback)
    }
}

#[cfg(test)]
mod tests;
