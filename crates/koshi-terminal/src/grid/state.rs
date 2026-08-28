//! The cell grid: the 2-D array of [`Cell`]s backing one screen buffer.

use std::cmp::min;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::style::Style;

/// The part of a cell that almost no cell has: the continuation code points
/// layered over its base character.
///
/// It is a type of its own so a [`Cell`] can hold it behind a *thin* pointer —
/// eight bytes, and null unless the cell actually has continuations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CellExtra {
    /// The continuation code points in arrival order. Never empty: the box is
    /// allocated only when the first one arrives.
    combining: Vec<char>,
}

/// A single grid cell: its character, display width, and style.
///
/// A cell occupies 32 bytes on a 64-bit target, and one exists per grid slot
/// and per scrollback-row column. The continuation code points sit behind a
/// pointer that is null for a plain cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    /// The base character occupying the cell.
    ch: char,
    /// The rest of the grapheme cluster layered over the base [`ch`](Cell::ch)
    /// — a grapheme cluster is the run of code points a person perceives as
    /// one visual character — in arrival order: combining accents, variation
    /// selectors, and the joined parts of a multi-codepoint emoji (ZWJ-joined
    /// glyphs, skin-tone modifiers, the second half of a flag). `None` for a
    /// plain cell; the renderer draws `ch` followed by these as one glyph.
    /// Named for the common case (combining marks) though it also carries
    /// non-zero-width emoji continuations.
    ///
    /// [`push_combining`](Cell::push_combining) is the only writer and always
    /// leaves at least one code point behind, so a present [`CellExtra`] is
    /// never empty and `None` is the single representation of "no
    /// continuations" — which is what makes the derived equality exact.
    combining: Option<Box<CellExtra>>,
    /// Display width in cells: 0 (continuation half of a wide glyph), 1
    /// (narrow), or 2 (wide, e.g. CJK).
    width: u8,
    /// The cell's visual style (color, bold, italic, etc.).
    style: Style,
}

/// Fails the build when [`Cell`] is not exactly 32 bytes on a 64-bit target.
///
/// One cell exists per grid slot and one per column of every row history
/// keeps: an 80×24 pane is 1 920 grid cells, and its scrollback adds up to
/// 10 000 rows on top of that.
///
/// **When it fires, the answer is usually [`CellExtra`], not a bigger number.**
/// That is what `combining` does: a plain cell holds eight bytes of null
/// pointer instead of a `Vec` inline. Rare per-cell data goes there, and a new
/// boolean attribute goes in one of
/// [`AttrFlags`](crate::style::AttrFlags)'s spare bits. Raising the figure
/// obligates raising it in the [`Cell`] doc in the same edit.
///
/// A 32-bit target holds that pointer in four bytes rather than eight, so this
/// is a 64-bit figure.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::size_of::<Cell>() == 32,
    "Cell changed size: put rare per-cell data behind CellExtra, or raise this figure and the `Cell` doc together"
);

impl Cell {
    /// A blank cell: a single space in the default style.
    pub fn blank() -> Self {
        Cell::blank_with(Style::default())
    }

    /// A blank cell — a single space — in the given `style`. Used to carry the
    /// current background into erased and scrolled cells (background-color
    /// erase); `style` is typically just the pen's background — the pen is
    /// the color/attribute state applied to newly written text.
    pub fn blank_with(style: Style) -> Self {
        Cell {
            ch: ' ',
            combining: None,
            width: 1,
            style,
        }
    }

    /// A cell holding `ch` of the given display `width`, in `style`.
    pub fn new(ch: char, width: u8, style: Style) -> Self {
        Cell {
            ch,
            combining: None,
            width,
            style,
        }
    }

    /// The character occupying this cell.
    pub fn ch(&self) -> char {
        self.ch
    }

    /// The rest of the grapheme cluster layered over the base character, in
    /// arrival order (combining marks plus any emoji continuation); empty for a
    /// plain cell.
    pub fn combining(&self) -> &[char] {
        match &self.combining {
            Some(extra) => &extra.combining,
            None => &[],
        }
    }

    /// Layer one continuation code point (combining mark, ZWJ, variation
    /// selector, joined emoji part, …) onto this cell, keeping the base
    /// character and width unchanged. The first mark allocates the backing
    /// vector; a plain cell never pays for one.
    pub fn push_combining(&mut self, mark: char) {
        self.combining
            .get_or_insert_with(|| {
                Box::new(CellExtra {
                    combining: Vec::new(),
                })
            })
            .combining
            .push(mark);
    }

    /// The cell's display width: 0 (combining/continuation), 1 (narrow), or 2
    /// (wide).
    pub fn width(&self) -> u8 {
        self.width
    }

    /// The cell's visual style.
    pub fn style(&self) -> Style {
        self.style
    }
}

/// How a row ends relative to the row directly below it. This is row state,
/// not cell state: it records whether the two rows hold one logical line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RowEnd {
    /// The row ends its logical line: the next row starts a new one.
    #[default]
    Hard,
    /// The row soft-wrapped under autowrap: the next row continues this
    /// row's logical line, and a resize reflow re-joins them.
    Soft,
    /// The row soft-wrapped because a wide glyph did not fit its last
    /// column: the final cell is a blank spacer, dropped when a reflow
    /// re-joins the line, so the wide glyph rejoins the text with no
    /// phantom space.
    SoftWide,
}

/// Everything the terminal records about a row apart from its cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RowMeta {
    /// How the row ends relative to the row below it.
    pub end: RowEnd,
    /// Whether a shell reported a prompt on this row with OSC 133;A.
    pub prompt: bool,
}

/// The number of content cells in a hard-ended row: its length with the
/// trailing run of fully-default blanks (the padding every row is filled
/// with) excluded. A styled blank — e.g. a background-colored prompt
/// segment — counts as content, so its color survives.
///
/// Only meaningful for a [`RowEnd::Hard`] row. A [`RowEnd::Soft`] row is full
/// of content by definition, and a [`RowEnd::SoftWide`] row's final blank is a
/// spacer standing in for the wide glyph on the next row, so neither may be
/// measured this way.
pub(crate) fn content_len(row: &[Cell]) -> usize {
    let blank = Cell::blank();
    row.iter()
        .rposition(|cell| *cell != blank)
        .map_or(0, |index| index + 1)
}

/// A fixed-size grid of cells, addressed `rows[row][col]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grid {
    /// Row-major cell storage: `rows[row][col]`.
    rows: Vec<Vec<Cell>>,
    /// Per-row metadata, parallel to `rows`. Every operation that adds,
    /// removes, or reorders rows maintains it.
    row_meta: Vec<RowMeta>,
}

impl Grid {
    /// Build a `rows × cols` grid, every cell a blank space in `fill`.
    pub fn blank(rows: u16, cols: u16, fill: Style) -> Self {
        Grid {
            rows: vec![vec![Cell::blank_with(fill); cols as usize]; rows as usize],
            row_meta: vec![RowMeta::default(); rows as usize],
        }
    }

    /// Build a grid from ready-made `rows`, normalizing each to exactly `cols`
    /// cells: a longer row is truncated, a shorter one padded with blank spaces
    /// in `fill` (both via [`Vec::resize`]). Every row starts with default
    /// metadata. Used to assemble a fresh screen or test grid.
    pub fn from_rows(rows: Vec<Vec<Cell>>, cols: u16, fill: Style) -> Self {
        let rows = rows
            .into_iter()
            .map(|row| (row, RowMeta::default()))
            .collect();
        Self::from_rows_with_meta(rows, cols, fill)
    }

    /// Build a grid from rows and their metadata, normalizing every row to
    /// exactly `cols` cells.
    pub(crate) fn from_rows_with_meta(
        mut rows: Vec<(Vec<Cell>, RowMeta)>,
        cols: u16,
        fill: Style,
    ) -> Self {
        for (row, _) in &mut rows {
            row.resize(cols as usize, Cell::blank_with(fill));
        }
        let (rows, row_meta): (Vec<Vec<Cell>>, Vec<RowMeta>) = rows.into_iter().unzip();
        Grid { rows, row_meta }
    }

    /// How `row` ends relative to the row below it; out of bounds reads as
    /// [`RowEnd::Hard`].
    pub fn row_end(&self, row: u16) -> RowEnd {
        self.row_meta
            .get(row as usize)
            .map_or(RowEnd::Hard, |meta| meta.end)
    }

    /// Record how `row` ends relative to the row below it. Out of bounds is a
    /// no-op.
    pub fn set_row_end(&mut self, row: u16, end: RowEnd) {
        if let Some(meta) = self.row_meta.get_mut(row as usize) {
            meta.end = end;
        }
    }

    /// Whether a shell reported a prompt on `row`; out of bounds reads false.
    pub fn prompt_mark(&self, row: u16) -> bool {
        self.row_meta
            .get(row as usize)
            .is_some_and(|meta| meta.prompt)
    }

    /// Set whether a shell reported a prompt on `row`. Out of bounds is a
    /// no-op.
    pub fn set_prompt_mark(&mut self, row: u16, prompt: bool) {
        if let Some(meta) = self.row_meta.get_mut(row as usize) {
            meta.prompt = prompt;
        }
    }

    /// The grid's dimensions as `(rows, cols)`.
    pub fn dimensions(&self) -> (u16, u16) {
        (
            self.rows.len() as u16,
            self.rows.first().map_or(0, Vec::len) as u16,
        )
    }

    /// A reference to the cell at (`row`, `col`), or `None` if out of bounds.
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell> {
        self.rows.get(row as usize)?.get(col as usize)
    }

    /// A mutable reference to the cell at (`row`, `col`), or `None` if out of
    /// bounds — the write path used by the VTE performer.
    pub fn cell_mut(&mut self, row: u16, col: u16) -> Option<&mut Cell> {
        self.rows.get_mut(row as usize)?.get_mut(col as usize)
    }

    /// All rows, row-major, for read-only iteration by the renderer.
    pub fn rows(&self) -> &[Vec<Cell>] {
        &self.rows
    }

    /// Blank columns `from..to` (half-open, `to` exclusive) in `row`, resetting
    /// each to a blank space in `fill`. An erase that reaches the row's last
    /// column breaks the row's continuation into the next row, so its end
    /// resets to [`RowEnd::Hard`]. The span is clipped to the row, so an
    /// oversized span, an inverted range (`from >= to`), or an empty grid never
    /// panics — it is simply a no-op.
    pub fn clear_line(&mut self, row: u16, from: u16, to: u16, fill: Style) {
        if let Some(cells) = self.rows.get_mut(row as usize) {
            let end = (to as usize).min(cells.len());
            if let Some(span) = cells.get_mut(from as usize..end) {
                span.fill(Cell::blank_with(fill));
            }
        }
        let (_, cols) = self.dimensions();
        if to >= cols && from < cols {
            self.set_row_end(row, RowEnd::Hard);
        }
    }

    /// Insert `n` blank cells at column `col` of `row`, shifting existing cells
    /// to the right; cells pushed past the right edge are dropped. If `row` or
    /// `col` are out of bounds, this is a no-op. The inserted cells are blanks
    /// in `fill` style (background-color erase).
    pub fn insert_cells(&mut self, row: u16, col: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if row >= rows || col >= cols {
            return;
        }

        let r = &mut self.rows[row as usize];

        r.splice(
            col as usize..col as usize,
            std::iter::repeat_n(Cell::blank_with(fill), n as usize),
        );
        r.truncate(cols as usize);
        // The shift replaced the row's tail, so any continuation into the
        // next row is broken.
        self.set_row_end(row, RowEnd::Hard);
    }

    /// Delete `n` cells starting at column `col` of `row`, shifting existing
    /// cells to the left; the freed space on the right is filled with blank cells
    /// in `fill` style (background-color erase). If `row` or `col` are out of
    /// bounds, this is a no-op.
    pub fn delete_cells(&mut self, row: u16, col: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if row >= rows || col >= cols {
            return;
        }

        let r = &mut self.rows[row as usize];
        let del = min(cols - col, n);

        r.drain(col as usize..(col + del) as usize);
        r.resize(cols as usize, Cell::blank_with(fill));
        // The shift replaced the row's tail, so any continuation into the
        // next row is broken.
        self.set_row_end(row, RowEnd::Hard);
    }

    /// Delete `n` lines from the band `[first, last]` (both inclusive), shifting
    /// lines below the band upward; blank lines are inserted at the bottom of the
    /// band to preserve the band's height. Cells are filled in `fill` style
    /// (background-color erase). Coordinates outside the grid are no-ops.
    pub fn delete_lines(&mut self, first: u16, last: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if first >= rows || last >= rows || first > last {
            return;
        }

        // Never remove more lines than the band actually holds.
        let remove_count = min(n, last - first + 1);

        // Each iteration removes the band's top line — the lines below it slide
        // up to fill the gap — blanks that line to `cols` cells in place, and
        // re-inserts it at the band's bottom, so the band keeps its original
        // height after every step and reuses the departing row's cell buffer.
        // Row metadata travels with each row, so a soft-wrapped row scrolled
        // off the top keeps its continuation state and prompt mark.
        for _ in 0..remove_count as usize {
            let mut recycled = self.rows.remove(first as usize);
            recycled.clear();
            recycled.resize(cols as usize, Cell::blank_with(fill));
            self.rows.insert(last as usize, recycled);
            self.row_meta.remove(first as usize);
            self.row_meta.insert(last as usize, RowMeta::default());
        }
        if remove_count > 0 {
            // The removed rows broke two continuations: the row above the
            // band lost the neighbor it wrapped into, and the row that slid
            // into the band's bottom now precedes a row it never wrapped into.
            if first > 0 {
                self.set_row_end(first - 1, RowEnd::Hard);
            }
            if let Some(slid_last) = last.checked_sub(remove_count) {
                self.set_row_end(slid_last, RowEnd::Hard);
            }
        }
    }

    /// Insert `n` blank lines within the band `[first, last]` (both inclusive),
    /// shifting lines downward; lines pushed below the band are dropped. Blank
    /// lines are filled in `fill` style (background-color erase). Coordinates
    /// outside the grid are no-ops.
    pub fn insert_lines(&mut self, first: u16, last: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if first >= rows || last >= rows || first > last {
            return;
        }

        // Never insert more lines than the band can hold.
        let insert_count = min(n, last - first + 1);

        // Each iteration removes the band's bottom line, blanks it to `cols`
        // cells in place, and re-inserts it at the band's top — the lines
        // between slide down — so the band keeps its original height after
        // every step and reuses the departing row's cell buffer. Row metadata
        // travels with each row.
        for _ in 0..insert_count as usize {
            let mut recycled = self.rows.remove(last as usize);
            recycled.clear();
            recycled.resize(cols as usize, Cell::blank_with(fill));
            self.rows.insert(first as usize, recycled);
            self.row_meta.remove(last as usize);
            self.row_meta.insert(first as usize, RowMeta::default());
        }
        // The inserted blanks broke two continuations: the row above the band
        // now precedes a blank row, and the row that slid into the band's
        // bottom now precedes a row it never wrapped into.
        if first > 0 {
            self.set_row_end(first - 1, RowEnd::Hard);
        }
        self.set_row_end(last, RowEnd::Hard);
    }
}

#[derive(Deserialize)]
struct GridFields {
    rows: Vec<Vec<Cell>>,
    #[serde(default)]
    row_meta: Option<Vec<RowMeta>>,
    #[serde(default)]
    row_ends: Option<Vec<RowEnd>>,
}

impl<'de> Deserialize<'de> for Grid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fields = GridFields::deserialize(deserializer)?;
        let row_meta = match (fields.row_meta, fields.row_ends) {
            (Some(row_meta), _) => row_meta,
            (None, Some(row_ends)) => row_ends
                .into_iter()
                .map(|end| RowMeta { end, prompt: false })
                .collect(),
            (None, None) => vec![RowMeta::default(); fields.rows.len()],
        };
        if row_meta.len() != fields.rows.len() {
            return Err(de::Error::custom("grid row metadata does not match rows"));
        }
        Ok(Grid {
            rows: fields.rows,
            row_meta,
        })
    }
}

#[cfg(test)]
mod tests;
