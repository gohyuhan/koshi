//! Per-pane scrollback history: a bounded buffer of lines that have scrolled
//! off the top of the primary screen.
//!
//! The buffer is capped on two axes — a maximum line count and a maximum byte
//! count — so a long-lived background pane cannot grow memory without bound.
//! When a push exceeds either cap the oldest lines are dropped from the front;
//! the count and byte size of everything dropped are tallied (never the content
//! itself) so the runtime can report truncation via
//! [`PaneScrollbackTruncated`](koshi_core::event::PaneScrollbackTruncated).

use std::collections::VecDeque;

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};

use crate::grid::state::{content_len, Cell, RowEnd, RowMeta};

/// Default scrollback line cap: 10 000 lines per pane.
const DEFAULT_MAX_LINES: usize = 10_000;
/// Default scrollback byte cap: 32 MiB of retained text per pane.
const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Drop the trailing run of fully-default blanks a row is padded out to the
/// screen width with, so history holds a line's text rather than a whole row
/// of cells. A 200-column row reading `README.md` keeps 9 cells instead of 200.
///
/// Only a [`RowEnd::Hard`] row is trimmed. A [`RowEnd::Soft`] row wrapped
/// because it filled the width, so every cell is content; a
/// [`RowEnd::SoftWide`] row's final blank is the spacer standing in for the
/// wide glyph that begins the next row. Both keep every cell, so their length
/// still marks where a reflow re-joins the logical line.
///
/// A styled blank — a background-colored prompt segment, say — is not a
/// default blank and is kept, so its color survives.
///
/// Returns a slice, so the caller allocates once at the size actually kept.
fn kept(row: &[Cell], end: RowEnd) -> &[Cell] {
    if matches!(end, RowEnd::Hard) {
        &row[..content_len(row)]
    } else {
        row
    }
}

/// Shorten an owned `row` to what history keeps of it, then hand back the
/// memory the dropped blanks held.
///
/// The counterpart of [`kept`] for a row already owned. It moves the cells it
/// keeps rather than cloning them, so a cell carrying combining marks does not
/// allocate a fresh copy of them, and a row that keeps every cell costs
/// nothing at all.
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
    /// The built-in caps applied when no configured limits are supplied: 10 000
    /// lines and 32 MiB of retained text.
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
    /// with its row metadata so a resize reflow can re-join soft-wrapped rows
    /// and carry prompt marks across the history/screen boundary.
    ///
    /// A row holds its text, not a whole screen line: the default blanks that
    /// padded it out to the screen width are dropped on the way in (see
    /// [`kept`]), so a row is as long as its content and reads as blank right
    /// of that.
    lines: VecDeque<(Vec<Cell>, RowMeta)>,
    /// Maximum rows retained before the oldest are dropped.
    max_lines: usize,
    /// Maximum total bytes (UTF-8 text payload) retained before the oldest rows
    /// are dropped.
    max_bytes: usize,
    /// Running sum of every retained row's byte size, kept incrementally so an
    /// overflow check is an O(1) comparison against this field.
    byte_total: usize,
    /// Cumulative count of rows ever pushed into the buffer; monotonic — a
    /// [`clear`](Self::clear) does not reset it. The runtime diffs it across a
    /// chunk to learn how many lines entered scrollback, re-anchoring
    /// scrolled-back views by exactly that many.
    total_pushed: u64,
    /// Cumulative count of rows ever dropped to honor the caps; monotonic.
    dropped_lines: u64,
    /// Cumulative bytes ever dropped to honor the caps; monotonic.
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

    /// The byte size of one row: every cell's base character plus its combining
    /// continuations, summed as UTF-8 lengths. This is the metric the byte cap
    /// is measured against.
    ///
    /// Width-0 cells are skipped: they are the placeholder right halves of wide
    /// (CJK/emoji) glyphs, which carry only a blank space. The glyph's real text
    /// lives entirely in its width-2 base cell, character plus combining marks.
    pub fn line_bytes(&self, line: &[Cell]) -> usize {
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

    /// Append `row` as the newest line — recording how it ended, so a reflow
    /// can re-join a soft-wrapped row with the screen row below it — then drop
    /// oldest rows from the front until both caps hold, tallying each drop.
    /// The byte cap never drops the sole remaining row (`lines.len() > 1`
    /// guard): a single row larger than `max_bytes` is still retained on
    /// arrival. The line cap has no such guard — the row count is always
    /// brought back under `max_lines`.
    ///
    /// A hard-ended row is stored without the trailing default blanks that pad
    /// it out to the screen width, so a 200-column row reading `README.md`
    /// keeps 9 cells. A soft-wrapped row keeps every cell. This method stores
    /// `prompt: false`; terminal output uses the metadata form when it has a
    /// prompt mark. Taking the row borrowed makes one allocation, at the stored
    /// size.
    pub fn push_row(&mut self, row: &[Cell], end: RowEnd) {
        self.push_row_with_meta(row, RowMeta { end, prompt: false });
    }

    /// Append `row` with its complete metadata. The terminal performer uses
    /// this when a row leaves the live grid.
    pub(crate) fn push_row_with_meta(&mut self, row: &[Cell], meta: RowMeta) {
        let line = kept(row, meta.end).to_vec();
        let new_bytes = self.line_bytes(&line);
        self.lines.push_back((line, meta));
        self.byte_total += new_bytes;
        self.total_pushed += 1;
        self.evict_to_caps();
    }

    /// Replace every retained row with `lines`, re-applying both caps — the
    /// resize reflow rebuilds history wholesale from the re-wrapped logical
    /// lines. Rows evicted by the caps are tallied as truncation like any
    /// other cap-driven drop. [`total_pushed`](Self::total_pushed) grows by
    /// the net increase in retained rows (rows the screen handed into
    /// history) and never decreases, staying monotonic.
    ///
    /// Each row is stored the same way [`push_row`](Self::push_row) stores
    /// one: a hard-ended row without its trailing default blanks, a
    /// soft-wrapped row whole. This method stores `prompt: false` for every
    /// row; terminal reflow uses the metadata form when it has prompt marks.
    /// Rows arrive owned, so they are shortened in place rather than copied.
    pub fn replace_lines(&mut self, lines: Vec<(Vec<Cell>, RowEnd)>) {
        self.replace_lines_with_meta(
            lines
                .into_iter()
                .map(|(cells, end)| (cells, RowMeta { end, prompt: false }))
                .collect(),
        );
    }

    /// Replace retained rows with their complete metadata.
    pub(crate) fn replace_lines_with_meta(&mut self, lines: Vec<(Vec<Cell>, RowMeta)>) {
        let before = self.lines.len() as u64;
        self.lines = lines
            .into_iter()
            .map(|(mut cells, meta)| {
                keep_in_place(&mut cells, meta.end);
                (cells, meta)
            })
            .collect();
        self.byte_total = self
            .lines
            .iter()
            .map(|(cells, _)| self.line_bytes(cells))
            .sum();
        self.evict_to_caps();
        let after = self.lines.len() as u64;
        self.total_pushed += after.saturating_sub(before);
    }

    /// Evict oldest rows one at a time, updating the running byte total and
    /// the truncation tallies, until both caps hold (or only one row is
    /// left, which the byte cap alone cannot evict).
    fn evict_to_caps(&mut self) {
        while self.lines.len() > self.max_lines
            || (self.byte_total > self.max_bytes && self.lines.len() > 1)
        {
            let (oldest_line, _) = self.lines.pop_front().unwrap();
            let oldest_bytes = self.line_bytes(&oldest_line);

            self.dropped_lines += 1;
            self.dropped_bytes += oldest_bytes as u64;
            self.byte_total -= oldest_bytes;
        }
    }

    /// Drop every retained row (xterm `CSI 3 J`, "erase saved lines"). The
    /// cumulative tallies are left intact — an explicit erase is not a
    /// cap-driven truncation — and [`total_pushed`](Self::total_pushed) stays
    /// monotonic across it.
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

    /// The retained rows with their metadata, oldest at the front. The
    /// terminal crate uses this to compose views and reflow wrapped lines.
    pub fn lines(&self) -> &VecDeque<(Vec<Cell>, RowMeta)> {
        &self.lines
    }

    /// Cumulative count of rows ever pushed into the buffer; monotonic — never
    /// reset, not even by [`clear`](Self::clear). Diffing it across a chunk gives
    /// the exact number of lines that entered scrollback in that chunk.
    pub fn total_pushed(&self) -> u64 {
        self.total_pushed
    }

    /// Cumulative count of rows dropped to honor the caps, for the runtime's
    /// [`PaneScrollbackTruncated`](koshi_core::event::PaneScrollbackTruncated)
    /// reporting.
    pub fn dropped_lines(&self) -> u64 {
        self.dropped_lines
    }

    /// Cumulative bytes dropped to honor the caps.
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

#[derive(Deserialize)]
struct ScrollbackFields {
    lines: VecDeque<SerializedLine>,
    max_lines: usize,
    max_bytes: usize,
    byte_total: usize,
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
        Ok(Scrollback {
            lines,
            max_lines: fields.max_lines,
            max_bytes: fields.max_bytes,
            byte_total: fields.byte_total,
            total_pushed: fields.total_pushed,
            dropped_lines: fields.dropped_lines,
            dropped_bytes: fields.dropped_bytes,
        })
    }
}

#[cfg(test)]
mod tests;
