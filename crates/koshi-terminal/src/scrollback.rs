//! Per-pane scrollback history: a bounded buffer of lines that have scrolled
//! off the top of the primary screen.
//!
//! The buffer is capped on two axes: a maximum row count and a maximum byte
//! count. When a push exceeds either cap the oldest rows are dropped from the
//! front.

use std::collections::VecDeque;
use std::fmt::{self, Formatter};
use std::sync::Arc;

use serde::de::{self, Deserializer as DeserializerTrait, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::grid::row_snapshot::RowSnapshot;
use crate::grid::state::{Cell, RowMetadata};
use crate::state::crop_columns;
use crate::style::Style;

/// Default scrollback line cap: 10 000 lines per pane.
const DEFAULT_MAX_LINE_COUNT: usize = 10_000;
/// Default scrollback byte cap: 32 MiB of retained text per pane.
const DEFAULT_MAX_BYTE_COUNT: usize = 32 * 1024 * 1024;

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

/// The scrollback buffer for one pane: a `VecDeque` of row snapshots (oldest
/// at the front), bounded by a line-count cap and a byte-count cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scrollback {
    /// Retained rows, oldest at the front and newest at the back. Each is the
    /// row's snapshot, with its row end and prompt mark, encoded at the
    /// primary screen's width.
    retained_lines: VecDeque<Arc<RowSnapshot>>,
    /// Maximum rows retained before the oldest are dropped.
    maximum_line_count: usize,
    /// Maximum total bytes (UTF-8 text payload) retained before the oldest rows
    /// are dropped.
    maximum_byte_count: usize,
    /// The sum of every retained row's text byte count
    /// ([`RowSnapshot::get_text_byte_count`]), updated on every push,
    /// replacement, eviction and clear.
    retained_byte_count: usize,
    /// Count of rows ever pushed into the buffer. It only grows:
    /// [`clear_scrollback`](Self::clear_scrollback) does not reset it.
    total_pushed_line_count: u64,
}

impl Scrollback {
    /// An empty buffer bounded by `scrollback_limit`.
    pub fn from_scrollback_limit(scrollback_limit: ScrollbackLimit) -> Self {
        Scrollback {
            retained_lines: VecDeque::new(),
            maximum_line_count: scrollback_limit.maximum_line_count,
            maximum_byte_count: scrollback_limit.maximum_byte_count,
            retained_byte_count: 0,
            total_pushed_line_count: 0,
        }
    }

    /// When the buffer holds `maximum_line_count` rows or more, drop the oldest
    /// one now — the row the next push drops at the line cap — and give back
    /// its encoded bytes for the next row to encode into. `evicted_line_callback`
    /// sees the dropped row. Gives back an empty buffer when the buffer is under
    /// the line cap, and when a frame still shares the dropped row.
    ///
    /// The next [`push_row_with_evicted`](Self::push_row_with_evicted) then
    /// keeps the same rows that it keeps without this call.
    pub(crate) fn evict_oldest_line_for_next_push(
        &mut self,
        evicted_line_callback: &mut impl FnMut(&RowSnapshot),
    ) -> Vec<u8> {
        if self.retained_lines.len() < self.maximum_line_count {
            return Vec::new();
        }
        let Some(oldest_line) = self.retained_lines.pop_front() else {
            return Vec::new();
        };
        evicted_line_callback(&oldest_line);
        self.retained_byte_count -= oldest_line.get_text_byte_count();
        Arc::try_unwrap(oldest_line)
            .map_or_else(|_| Vec::new(), RowSnapshot::into_encoded_row_bytes)
    }

    /// Append `row_snapshot` as the newest line, then drop the oldest rows
    /// from the front until both caps hold, calling `evicted_line_callback` on
    /// each dropped row, oldest first. The byte cap never drops the sole
    /// remaining row: a single row larger than `maximum_byte_count` is
    /// retained on arrival. The line cap has no such guard; the row count
    /// always ends at or under `maximum_line_count`.
    pub(crate) fn push_row_with_evicted(
        &mut self,
        row_snapshot: Arc<RowSnapshot>,
        mut evicted_line_callback: impl FnMut(&RowSnapshot),
    ) {
        self.retained_byte_count += row_snapshot.get_text_byte_count();
        self.retained_lines.push_back(row_snapshot);
        self.total_pushed_line_count += 1;
        self.evict_oldest_lines_to_limits(&mut evicted_line_callback);
    }

    /// Remove and return every retained row, oldest at the front, leaving the
    /// buffer empty with a zero byte total. The caps and
    /// [`total_pushed_line_count`](Self::total_pushed_line_count) keep their
    /// values. The caller passes the returned rows' count to
    /// [`replace_retained_lines`](Self::replace_retained_lines) as `retained_line_count_before`.
    pub(crate) fn take_retained_lines(&mut self) -> VecDeque<Arc<RowSnapshot>> {
        self.retained_byte_count = 0;
        std::mem::take(&mut self.retained_lines)
    }

    /// Replace every retained row with `retained_lines`, then apply both caps.
    /// [`total_pushed_line_count`](Self::total_pushed_line_count) grows by the count of retained
    /// rows (counted after eviction) exceeding `retained_line_count_before` and never
    /// decreases.
    pub(crate) fn replace_retained_lines(
        &mut self,
        retained_lines: Vec<Arc<RowSnapshot>>,
        retained_line_count_before: u64,
    ) {
        self.retained_lines = VecDeque::from(retained_lines);
        self.retained_byte_count = compute_retained_byte_count(&self.retained_lines);
        self.evict_oldest_lines_to_limits(&mut |_| {});
        let retained_line_count_after = self.retained_lines.len() as u64;
        self.total_pushed_line_count +=
            retained_line_count_after.saturating_sub(retained_line_count_before);
    }

    /// Drop the oldest row, update `retained_byte_count`, and
    /// repeat while the row count exceeds `maximum_line_count`, or while `retained_byte_count`
    /// exceeds `maximum_byte_count` and more than one row remains.
    fn evict_oldest_lines_to_limits(
        &mut self,
        evicted_line_callback: &mut impl FnMut(&RowSnapshot),
    ) {
        while self.retained_lines.len() > self.maximum_line_count
            || (self.retained_byte_count > self.maximum_byte_count && self.retained_lines.len() > 1)
        {
            let Some(oldest_line) = self.retained_lines.pop_front() else {
                return;
            };
            evicted_line_callback(&oldest_line);
            self.retained_byte_count -= oldest_line.get_text_byte_count();
        }
    }

    /// Drop every retained row (xterm `CSI 3 J`, "erase saved lines") and zero
    /// `retained_byte_count`.
    /// [`get_total_pushed_line_count`](Self::get_total_pushed_line_count) keeps its value.
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

    /// The retained rows, oldest at the front.
    pub fn list_retained_lines(&self) -> &VecDeque<Arc<RowSnapshot>> {
        &self.retained_lines
    }

    /// Count of rows ever pushed into the buffer. It never decreases;
    /// [`clear_scrollback`](Self::clear_scrollback) does not reset it.
    pub fn get_total_pushed_line_count(&self) -> u64 {
        self.total_pushed_line_count
    }
}

/// The stored form of a [`Scrollback`], as [`Deserialize`] reads it.
///
/// `retained_byte_count` is not read: the loaded buffer's byte total is the
/// sum of the text byte counts of `retained_lines`. The caps apply to the rows
/// that were read: a stored buffer holding more than `maximum_line_count` rows
/// loses its oldest ones at load.
#[derive(Deserialize)]
struct ScrollbackFields {
    retained_lines: VecDeque<Arc<RowSnapshot>>,
    maximum_line_count: usize,
    maximum_byte_count: usize,
    total_pushed_line_count: u64,
}

/// The sum of the text byte counts ([`RowSnapshot::get_text_byte_count`]) of
/// every row of `retained_lines`.
fn compute_retained_byte_count(retained_lines: &VecDeque<Arc<RowSnapshot>>) -> usize {
    retained_lines
        .iter()
        .map(|retained_line| retained_line.get_text_byte_count())
        .sum()
}

impl Scrollback {
    /// The buffer holding `retained_lines` under both caps, with the byte
    /// total derived from the lines. A buffer holding more lines than the caps
    /// allow loses its oldest ones.
    fn from_stored_lines(
        retained_lines: VecDeque<Arc<RowSnapshot>>,
        maximum_line_count: usize,
        maximum_byte_count: usize,
        total_pushed_line_count: u64,
    ) -> Self {
        let retained_byte_count = compute_retained_byte_count(&retained_lines);
        let mut scrollback = Scrollback {
            retained_lines,
            maximum_line_count,
            maximum_byte_count,
            retained_byte_count,
            total_pushed_line_count,
        };
        scrollback.evict_oldest_lines_to_limits(&mut |_| {});
        scrollback
    }

    /// Bring each line that holds another count of cells than `column_count`
    /// to exactly `column_count` cells: default blanks pad a shorter line, and
    /// a longer line loses its cells past `column_count`. A wide glyph whose
    /// right half falls past `column_count` becomes a default blank. Each line
    /// keeps its row metadata, and the byte total is counted again.
    ///
    /// Example — at `column_count` 5, the hard line `abc` becomes `abc` and
    /// two default blanks, the soft line `abcdefg` becomes `abcde`, and the
    /// soft line `abcd漢` becomes `abcd` and one default blank.
    pub(crate) fn fit_lines_to_column_count(&mut self, column_count: u16) {
        for retained_line in &mut self.retained_lines {
            if retained_line.get_encoded_row_shape().column_count == column_count {
                continue;
            }
            let row_metadata = retained_line.get_row_metadata();
            let mut row_cells = retained_line.decode_row_cells();
            crop_columns(&mut row_cells, column_count, Style::default());
            *retained_line = Arc::new(RowSnapshot::from_row_cells(
                &row_cells,
                row_metadata,
                Vec::new(),
            ));
        }
        self.retained_byte_count = compute_retained_byte_count(&self.retained_lines);
    }

    /// Whether every line holds exactly `column_count` cells.
    pub(crate) fn has_only_lines_of_column_count(&self, column_count: u16) -> bool {
        self.retained_lines
            .iter()
            .all(|retained_line| retained_line.get_encoded_row_shape().column_count == column_count)
    }
}

impl<'de> Deserialize<'de> for Scrollback {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        let serialized_fields = ScrollbackFields::deserialize(deserializer)?;
        Ok(Scrollback::from_stored_lines(
            serialized_fields.retained_lines,
            serialized_fields.maximum_line_count,
            serialized_fields.maximum_byte_count,
            serialized_fields.total_pushed_line_count,
        ))
    }
}

/// A history stored with each line as a pair of its JSON cells and its row
/// metadata, as resume format 4 writes it. Each line is encoded as it is read.
pub(crate) struct CellRowScrollback(pub(crate) Scrollback);

/// The stored form of a [`CellRowScrollback`]. The fields mean what the
/// fields of [`ScrollbackFields`] mean.
#[derive(Deserialize)]
struct CellRowScrollbackFields {
    retained_lines: CellRowLines,
    maximum_line_count: usize,
    maximum_byte_count: usize,
    total_pushed_line_count: u64,
}

/// History lines read from pairs of JSON cells and row metadata, oldest first,
/// each encoded as it is read. A line of more than 65 535 cells does not
/// read.
struct CellRowLines(VecDeque<Arc<RowSnapshot>>);

impl<'de> Deserialize<'de> for CellRowLines {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        struct CellRowLinesVisitor;

        impl<'de> Visitor<'de> for CellRowLinesVisitor {
            type Value = CellRowLines;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                formatter.write_str("a list of history lines")
            }

            fn visit_seq<LineAccess>(
                self,
                mut line_access: LineAccess,
            ) -> Result<Self::Value, LineAccess::Error>
            where
                LineAccess: SeqAccess<'de>,
            {
                let mut retained_lines = VecDeque::new();
                while let Some((row_cells, row_metadata)) =
                    line_access.next_element::<(Vec<Cell>, RowMetadata)>()?
                {
                    if row_cells.len() > usize::from(u16::MAX) {
                        return Err(de::Error::custom(
                            "a history line holds more than 65 535 cells",
                        ));
                    }
                    retained_lines.push_back(Arc::new(RowSnapshot::from_row_cells(
                        &row_cells,
                        row_metadata,
                        Vec::new(),
                    )));
                }
                Ok(CellRowLines(retained_lines))
            }
        }

        deserializer.deserialize_seq(CellRowLinesVisitor)
    }
}

impl<'de> Deserialize<'de> for CellRowScrollback {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        let serialized_fields = CellRowScrollbackFields::deserialize(deserializer)?;
        Ok(CellRowScrollback(Scrollback::from_stored_lines(
            serialized_fields.retained_lines.0,
            serialized_fields.maximum_line_count,
            serialized_fields.maximum_byte_count,
            serialized_fields.total_pushed_line_count,
        )))
    }
}

#[cfg(test)]
pub(crate) mod tests;
