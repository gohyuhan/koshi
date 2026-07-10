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

use crate::grid::state::Cell;

/// Default scrollback line cap: 10 000 lines per pane.
const DEFAULT_MAX_LINES: usize = 10_000;
/// Default scrollback byte cap: 32 MiB of retained text per pane.
const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;

/// The line- and byte-count caps bounding one pane's [`Scrollback`].
#[derive(Debug, Clone, Copy)]
pub struct ScrollbackLimit {
    max_lines: usize,
    max_bytes: usize,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scrollback {
    /// Retained rows, oldest at the front and newest at the back. Each row is a
    /// full grid line captured as it scrolled off the top.
    lines: VecDeque<Vec<Cell>>,
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
    /// (CJK/emoji) glyphs, which carry only a blank space — the glyph's real
    /// text lives entirely in its width-2 base cell (character plus combining
    /// marks). Counting the placeholder would over-charge one byte per wide
    /// glyph and evict history early for wide-glyph-heavy output.
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

    /// Append `line` as the newest row, then drop oldest rows from the front
    /// until both caps hold, tallying each drop. The byte cap never drops the
    /// sole remaining row (`lines.len() > 1` guard): a single row larger than
    /// `max_bytes` is still retained on arrival. The line cap has no such
    /// guard — the row count is always brought back under `max_lines`.
    pub fn push_line(&mut self, line: Vec<Cell>) {
        let new_bytes = self.line_bytes(&line);
        self.lines.push_back(line);
        self.byte_total += new_bytes;
        self.total_pushed += 1;

        // Evict oldest rows one at a time, updating the running byte total and
        // the truncation tallies, until both caps hold (or only one row is
        // left, which the byte cap alone cannot evict).
        while self.lines.len() > self.max_lines
            || (self.byte_total > self.max_bytes && self.lines.len() > 1)
        {
            let oldest_line = self.lines.pop_front().unwrap();
            let oldest_bytes = self.line_bytes(&oldest_line);

            self.dropped_lines += 1;
            self.dropped_bytes += oldest_bytes as u64;
            self.byte_total -= oldest_bytes;
        }
    }

    /// Drop every retained row (xterm `CSI 3 J`, "erase saved lines"). The
    /// cumulative tallies are left intact: an explicit erase is not a cap-driven
    /// truncation, so it must not perturb the truncation reporting, and
    /// [`total_pushed`](Self::total_pushed) stays monotonic across it.
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

    /// The retained rows, oldest at the front. Lets the renderer compose a
    /// scrolled-back view above the live grid.
    pub fn lines(&self) -> &VecDeque<Vec<Cell>> {
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

#[cfg(test)]
mod tests;
