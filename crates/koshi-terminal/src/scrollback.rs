//! Per-pane scrollback history: a bounded buffer of lines that have scrolled
//! off the top of the primary screen.
//!
//! The buffer is capped on two axes: a maximum row count and a maximum byte
//! count. When a push exceeds either cap the oldest rows are dropped from the
//! front. The count and byte size of everything dropped are tallied, never the
//! content itself. A snapshot reads the row tally as one boolean:
//! `dropped_lines() > 0` becomes `ScrollbackMeta::truncated`.

use std::collections::VecDeque;

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

use crate::grid::state::{content_len, Cell, RowEnd, RowMeta};

/// Default scrollback line cap: 10 000 lines per pane.
const DEFAULT_MAX_LINES: usize = 10_000;
/// Default scrollback byte cap: 32 MiB of retained text per pane.
const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;

/// The cells history keeps of `row`: a [`RowEnd::Hard`] row without the
/// trailing run of fully-default blanks, every other row whole.
///
/// A 200-column hard row reading `README.md` keeps 9 cells. A styled blank —
/// a background-colored prompt segment — is not a default blank and is kept.
/// A [`RowEnd::Soft`] row keeps every cell. A [`RowEnd::SoftWide`] row keeps
/// every cell, including the final blank spacer that stands in for the wide
/// glyph on the next row.
fn kept(row: &[Cell], end: RowEnd) -> &[Cell] {
    if end == RowEnd::Hard {
        &row[..content_len(row)]
    } else {
        row
    }
}

/// The byte size of one row: the UTF-8 length of every cell's base character
/// plus its combining marks, summed over the cells whose width is not `0`. The
/// byte cap is measured in this unit.
///
/// A width-0 cell is the placeholder right half of a wide glyph and adds
/// nothing; the glyph's text is counted in its width-2 base cell.
fn line_bytes(line: &[Cell]) -> usize {
    line.iter()
        .filter(|cell| cell.width() != 0)
        .map(|cell| {
            cell.ch().len_utf8()
                + cell
                    .combining()
                    .iter()
                    .map(|combining| combining.len_utf8())
                    .sum::<usize>()
        })
        .sum()
}

/// Truncate an owned `row` to what [`kept`] keeps of it and release the spare
/// capacity.
fn keep_in_place(row: &mut Vec<Cell>, end: RowEnd) {
    row.truncate(kept(row, end).len());
    row.shrink_to_fit();
}

/// The line- and byte-count caps bounding one pane's [`Scrollback`].
#[derive(Debug, Clone, Copy)]
pub struct ScrollbackLimit {
    max_lines: usize,
    max_bytes: usize,
}

impl ScrollbackLimit {
    /// A cap of exactly `max_lines` rows and `max_bytes` bytes.
    pub fn new(max_lines: usize, max_bytes: usize) -> Self {
        ScrollbackLimit {
            max_lines,
            max_bytes,
        }
    }
}

impl Default for ScrollbackLimit {
    /// 10 000 lines and 32 MiB of retained text.
    fn default() -> Self {
        ScrollbackLimit {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// The scrollback buffer for one pane: a `VecDeque` of rows (oldest at the
/// front), bounded by line- and byte-count caps with truncation accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scrollback {
    /// Retained rows, oldest at the front and newest at the back, each paired
    /// with its row metadata. A row holds what [`kept`] keeps of it: a
    /// hard-ended row stops at its last content cell and reads as blank right
    /// of that.
    lines: VecDeque<(Vec<Cell>, RowMeta)>,
    /// Maximum rows retained before the oldest are dropped.
    max_lines: usize,
    /// Maximum total bytes (UTF-8 text payload) retained before the oldest rows
    /// are dropped.
    max_bytes: usize,
    /// The sum of [`line_bytes`] over every retained row, updated on every
    /// push, replacement, eviction and clear.
    byte_total: usize,
    /// Count of rows ever pushed into the buffer. It only grows:
    /// [`clear`](Self::clear) does not reset it.
    total_pushed: u64,
    /// Count of rows dropped to honor the caps. It only grows.
    dropped_lines: u64,
    /// Bytes dropped to honor the caps. It only grows.
    dropped_bytes: u64,
}

impl Scrollback {
    /// An empty buffer bounded by `limit`.
    pub fn new(limit: ScrollbackLimit) -> Self {
        Scrollback {
            lines: VecDeque::new(),
            max_lines: limit.max_lines,
            max_bytes: limit.max_bytes,
            byte_total: 0,
            total_pushed: 0,
            dropped_lines: 0,
            dropped_bytes: 0,
        }
    }

    /// Append `row` as the newest line with `meta`, then drop the oldest rows
    /// from the front until both caps hold, tallying each drop. The byte cap
    /// never drops the sole remaining row: a single row larger than
    /// `max_bytes` is retained on arrival. The line cap has no such guard; the
    /// row count always ends at or under `max_lines`.
    ///
    /// A hard-ended row is stored without its trailing run of fully-default
    /// blanks: a 200-column row reading `README.md` keeps 9 cells. A
    /// soft-wrapped row keeps every cell. One allocation, at the stored size.
    pub(crate) fn push_row(&mut self, row: &[Cell], meta: RowMeta) {
        let line = kept(row, meta.end).to_vec();
        let new_bytes = line_bytes(&line);
        self.lines.push_back((line, meta));
        self.byte_total += new_bytes;
        self.total_pushed += 1;
        self.evict_to_caps();
    }

    /// Remove and return every retained row with its metadata, oldest at the
    /// front, leaving the buffer empty with a zero byte total. The caps, the
    /// dropped tallies, and [`total_pushed`](Self::total_pushed) keep their
    /// values. The caller passes the returned rows' count to
    /// [`replace_lines`](Self::replace_lines) as `retained_before`.
    pub(crate) fn take_lines(&mut self) -> VecDeque<(Vec<Cell>, RowMeta)> {
        self.byte_total = 0;
        std::mem::take(&mut self.lines)
    }

    /// Replace every retained row with `lines`, each keeping its own metadata,
    /// then apply both caps. Rows the caps evict are tallied as dropped.
    /// [`total_pushed`](Self::total_pushed) grows by the count of retained
    /// rows (counted after eviction) exceeding `retained_before` and never
    /// decreases.
    ///
    /// Each row is stored the way [`push_row`](Self::push_row) stores one,
    /// shortened in place: a hard-ended row without its trailing default
    /// blanks, a soft-wrapped row whole.
    pub(crate) fn replace_lines(&mut self, lines: Vec<(Vec<Cell>, RowMeta)>, retained_before: u64) {
        self.lines = lines
            .into_iter()
            .map(|(mut cells, meta)| {
                keep_in_place(&mut cells, meta.end);
                (cells, meta)
            })
            .collect();
        self.byte_total = self.lines.iter().map(|(cells, _)| line_bytes(cells)).sum();
        self.evict_to_caps();
        let after = self.lines.len() as u64;
        self.total_pushed += after.saturating_sub(retained_before);
    }

    /// Drop the oldest row, update `byte_total` and the dropped tallies, and
    /// repeat while the row count exceeds `max_lines`, or while `byte_total`
    /// exceeds `max_bytes` and more than one row remains.
    fn evict_to_caps(&mut self) {
        while self.lines.len() > self.max_lines
            || (self.byte_total > self.max_bytes && self.lines.len() > 1)
        {
            let (oldest_line, _) = self.lines.pop_front().unwrap();
            let oldest_bytes = line_bytes(&oldest_line);

            self.dropped_lines += 1;
            self.dropped_bytes += oldest_bytes as u64;
            self.byte_total -= oldest_bytes;
        }
    }

    /// Drop every retained row (xterm `CSI 3 J`, "erase saved lines") and zero
    /// `byte_total`. The dropped tallies and
    /// [`total_pushed`](Self::total_pushed) keep their values.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.byte_total = 0;
    }

    /// The number of rows currently retained.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Whether the buffer retains no rows.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The retained rows with their metadata, oldest at the front.
    pub fn lines(&self) -> &VecDeque<(Vec<Cell>, RowMeta)> {
        &self.lines
    }

    /// Count of rows ever pushed into the buffer. It never decreases;
    /// [`clear`](Self::clear) does not reset it.
    pub fn total_pushed(&self) -> u64 {
        self.total_pushed
    }

    /// Count of rows dropped to honor the caps.
    pub fn dropped_lines(&self) -> u64 {
        self.dropped_lines
    }

    /// Bytes dropped to honor the caps.
    pub fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SerializedLine {
    Current((Vec<Cell>, RowMeta)),
    Legacy((Vec<Cell>, RowEnd)),
}

/// The stored form of a [`Scrollback`], as [`Deserialize`] reads it.
///
/// `byte_total` is not read: it is derived from `lines` instead, so a stored
/// total that does not match the rows cannot underflow the first eviction.
/// The caps are applied to the rows that were read, so a stored buffer holding
/// more than `max_lines` rows loses its oldest ones at load and tallies them
/// as dropped.
#[derive(Deserialize)]
struct ScrollbackFields {
    lines: VecDeque<SerializedLine>,
    max_lines: usize,
    max_bytes: usize,
    total_pushed: u64,
    dropped_lines: u64,
    dropped_bytes: u64,
}

impl<'de> Deserialize<'de> for Scrollback {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fields = ScrollbackFields::deserialize(deserializer)?;
        let lines = fields
            .lines
            .into_iter()
            .map(|line| match line {
                SerializedLine::Current((cells, meta)) => (cells, meta),
                SerializedLine::Legacy((cells, end)) => (cells, RowMeta { end, prompt: false }),
            })
            .collect();
        let mut scrollback = Scrollback {
            lines,
            max_lines: fields.max_lines,
            max_bytes: fields.max_bytes,
            byte_total: 0,
            total_pushed: fields.total_pushed,
            dropped_lines: fields.dropped_lines,
            dropped_bytes: fields.dropped_bytes,
        };
        scrollback.byte_total = scrollback
            .lines
            .iter()
            .map(|(cells, _)| line_bytes(cells))
            .sum();
        scrollback.evict_to_caps();
        Ok(scrollback)
    }
}

#[cfg(test)]
mod tests;
