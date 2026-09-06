//! The cell grid: the 2-D array of [`Cell`]s backing one screen buffer.

use std::cmp::min;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::style::{Color, Style};

/// The part of a cell that almost no cell has: continuation code points or
/// Kitty placeholder metadata. A [`Cell`] holds it behind one pointer, eight
/// bytes on a 64-bit target, null for ordinary cells.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CellExtra {
    /// The continuation code points in arrival order.
    combining: Vec<char>,
    /// Kitty Unicode-placeholder metadata, when the base cell is a placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_placeholder: Option<ImagePlaceholder>,
}

/// The image identity and source cell encoded by a Kitty Unicode placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImagePlaceholder {
    /// The low 24 bits encoded by the cell foreground color.
    pub(crate) image_id: u32,
    /// The optional placement id encoded by the underline color.
    pub(crate) placement_id: Option<u32>,
    /// The source row encoded by the first placeholder diacritic.
    pub(crate) row: Option<u16>,
    /// The source column encoded by the second placeholder diacritic.
    pub(crate) column: Option<u16>,
    /// The most significant image-id byte encoded by the third diacritic.
    pub(crate) image_id_msb: Option<u8>,
}

impl ImagePlaceholder {
    /// Build placeholder metadata from Kitty's foreground and underline colors.
    pub(crate) fn from_style(style: Style) -> Self {
        Self {
            image_id: color_value(style.fg()).unwrap_or_default(),
            placement_id: style
                .underline_color()
                .and_then(color_value)
                .filter(|value| *value != 0),
            row: None,
            column: None,
            image_id_msb: None,
        }
    }
}

fn color_value(color: Color) -> Option<u32> {
    match color {
        Color::Default => None,
        Color::Indexed(value) => Some(u32::from(value)),
        Color::Rgb(red, green, blue) => {
            Some((u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue))
        }
    }
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
    ///
    /// [`push_combining`](Cell::push_combining) is the normal writer; a
    /// placeholder can allocate the same storage without continuation marks.
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
/// Rare per-cell data goes behind [`CellExtra`]; a new boolean attribute goes
/// in one of [`AttrFlags`](crate::style::AttrFlags)'s spare bits. Raising the
/// figure obligates raising it in the [`Cell`] doc in the same edit.
///
/// A 32-bit target holds the [`CellExtra`] pointer in four bytes; the check
/// runs on 64-bit targets only.
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

    /// A blank cell — a single space — in the given `style`. Erased and
    /// scrolled cells are built this way with the pen's background
    /// (background-color erase); the pen is the color and attribute state
    /// applied to newly written text.
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

    /// Return the Kitty Unicode-placeholder metadata carried by this cell.
    pub(crate) fn image_placeholder(&self) -> Option<ImagePlaceholder> {
        self.combining
            .as_ref()
            .and_then(|extra| extra.image_placeholder)
    }

    /// Return whether this cell carries a Kitty Unicode-placeholder marker.
    #[must_use]
    pub fn has_image_placeholder(&self) -> bool {
        self.image_placeholder().is_some()
    }

    /// Set the Kitty Unicode-placeholder metadata and use a blank base glyph.
    pub(crate) fn set_image_placeholder(&mut self, placeholder: ImagePlaceholder) {
        self.ch = ' ';
        self.combining
            .get_or_insert_with(|| {
                Box::new(CellExtra {
                    combining: Vec::new(),
                    image_placeholder: None,
                })
            })
            .image_placeholder = Some(placeholder);
    }

    /// Add one Kitty placeholder diacritic to the matching metadata slot.
    pub(crate) fn set_image_placeholder_diacritic(&mut self, mark: char) -> bool {
        let Some(extra) = self.combining.as_mut() else {
            return false;
        };
        let Some(placeholder) = extra.image_placeholder.as_mut() else {
            return false;
        };
        let Some(index) = image_placeholder_diacritic_index(mark) else {
            return false;
        };
        if placeholder.row.is_none() {
            placeholder.row = Some(index);
        } else if placeholder.column.is_none() {
            placeholder.column = Some(index);
        } else if placeholder.image_id_msb.is_none() {
            placeholder.image_id_msb = u8::try_from(index).ok();
        }
        true
    }

    /// Layer one continuation code point (combining mark, ZWJ, variation
    /// selector, joined emoji part, …) onto this cell, keeping the base
    /// character and width unchanged. The first mark allocates the backing
    /// vector.
    pub fn push_combining(&mut self, mark: char) {
        self.combining
            .get_or_insert_with(|| {
                Box::new(CellExtra {
                    combining: Vec::new(),
                    image_placeholder: None,
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

const IMAGE_PLACEHOLDER_DIACRITICS: [char; 256] = [
    '\u{0305}', '\u{030d}', '\u{030e}', '\u{0310}', '\u{0312}', '\u{033d}', '\u{033e}', '\u{033f}',
    '\u{0346}', '\u{034a}', '\u{034b}', '\u{034c}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035b}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036a}', '\u{036b}', '\u{036c}', '\u{036d}', '\u{036e}', '\u{036f}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059c}', '\u{059d}', '\u{059e}', '\u{059f}', '\u{05a0}', '\u{05a1}',
    '\u{05a8}', '\u{05a9}', '\u{05ab}', '\u{05ac}', '\u{05af}', '\u{05c4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065a}', '\u{065b}', '\u{065d}', '\u{065e}', '\u{06d6}', '\u{06d7}', '\u{06d8}',
    '\u{06d9}', '\u{06da}', '\u{06db}', '\u{06dc}', '\u{06df}', '\u{06e0}', '\u{06e1}', '\u{06e2}',
    '\u{06e4}', '\u{06e7}', '\u{06e8}', '\u{06eb}', '\u{06ec}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073a}', '\u{073d}', '\u{073f}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074a}', '\u{07eb}', '\u{07ec}', '\u{07ed}', '\u{07ee}',
    '\u{07ef}', '\u{07f0}', '\u{07f1}', '\u{07f3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081b}', '\u{081c}', '\u{081d}', '\u{081e}', '\u{081f}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082a}', '\u{082b}', '\u{082c}',
    '\u{082d}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0f82}', '\u{0f83}', '\u{0f86}', '\u{0f87}',
    '\u{135d}', '\u{135e}', '\u{135f}', '\u{17dd}', '\u{193a}', '\u{1a17}', '\u{1a75}', '\u{1a76}',
    '\u{1a77}', '\u{1a78}', '\u{1a79}', '\u{1a7a}', '\u{1a7b}', '\u{1a7c}', '\u{1b6b}', '\u{1b6d}',
    '\u{1b6e}', '\u{1b6f}', '\u{1b70}', '\u{1b71}', '\u{1b72}', '\u{1b73}', '\u{1cd0}', '\u{1cd1}',
    '\u{1cd2}', '\u{1cda}', '\u{1cdb}', '\u{1ce0}', '\u{1dc0}', '\u{1dc1}', '\u{1dc3}', '\u{1dc4}',
    '\u{1dc5}', '\u{1dc6}', '\u{1dc7}', '\u{1dc8}', '\u{1dc9}', '\u{1dcb}', '\u{1dcc}', '\u{1dd1}',
    '\u{1dd2}', '\u{1dd3}', '\u{1dd4}', '\u{1dd5}', '\u{1dd6}', '\u{1dd7}', '\u{1dd8}', '\u{1dd9}',
    '\u{1dda}', '\u{1ddb}', '\u{1ddc}', '\u{1ddd}', '\u{1dde}', '\u{1ddf}', '\u{1de0}', '\u{1de1}',
    '\u{1de2}', '\u{1de3}', '\u{1de4}', '\u{1de5}', '\u{1de6}', '\u{1dfe}', '\u{20d0}', '\u{20d1}',
    '\u{20d4}', '\u{20d5}', '\u{20d6}', '\u{20d7}', '\u{20db}', '\u{20dc}', '\u{20e1}', '\u{20e7}',
    '\u{20e9}', '\u{20f0}', '\u{2cef}', '\u{2cf0}', '\u{2cf1}', '\u{2de0}', '\u{2de1}', '\u{2de2}',
    '\u{2de3}', '\u{2de4}', '\u{2de5}', '\u{2de6}', '\u{2de7}', '\u{2de8}', '\u{2de9}', '\u{2dea}',
    '\u{2deb}', '\u{2dec}', '\u{2ded}', '\u{2dee}', '\u{2def}', '\u{2df0}', '\u{2df1}', '\u{2df2}',
    '\u{2df3}', '\u{2df4}', '\u{2df5}', '\u{2df6}', '\u{2df7}', '\u{2df8}', '\u{2df9}', '\u{2dfa}',
    '\u{2dfb}', '\u{2dfc}', '\u{2dfd}', '\u{2dfe}', '\u{2dff}', '\u{a66f}', '\u{a67c}', '\u{a67d}',
    '\u{a6f0}', '\u{a6f1}', '\u{a8e0}', '\u{a8e1}', '\u{a8e2}', '\u{a8e3}', '\u{a8e4}', '\u{a8e5}',
];

fn image_placeholder_diacritic_index(mark: char) -> Option<u16> {
    IMAGE_PLACEHOLDER_DIACRITICS
        .iter()
        .position(|candidate| *candidate == mark)
        .and_then(|index| u16::try_from(index).ok())
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
    /// The row soft-wrapped when a wide glyph did not fit its last column:
    /// the final cell is a blank spacer, dropped when a reflow re-joins the
    /// line.
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
/// segment — counts as content.
///
/// Only meaningful for a [`RowEnd::Hard`] row. A [`RowEnd::Soft`] row is full
/// of content, and a [`RowEnd::SoftWide`] row's final blank is a spacer
/// standing in for the wide glyph on the next row.
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
    /// in `fill`. Every row starts with default metadata.
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

    /// Everything recorded about `row` apart from its cells; out of bounds
    /// reads as [`RowMeta::default`] — a [`RowEnd::Hard`] end and no prompt
    /// mark.
    pub fn row_meta(&self, row: u16) -> RowMeta {
        self.row_meta.get(row as usize).copied().unwrap_or_default()
    }

    /// How `row` ends relative to the row below it; out of bounds reads as
    /// [`RowEnd::Hard`].
    pub fn row_end(&self, row: u16) -> RowEnd {
        self.row_meta(row).end
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
        self.row_meta(row).prompt
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
    /// bounds.
    pub fn cell_mut(&mut self, row: u16, col: u16) -> Option<&mut Cell> {
        self.rows.get_mut(row as usize)?.get_mut(col as usize)
    }

    /// All rows, row-major.
    pub fn rows(&self) -> &[Vec<Cell>] {
        &self.rows
    }

    /// Blank columns `from..to` (half-open, `to` exclusive) in `row`, resetting
    /// each to a blank space in `fill`. When the span reaches the row's last
    /// column (`from < cols` and `to >= cols`), the row's end resets to
    /// [`RowEnd::Hard`]. The span is clipped to the row: an oversized span
    /// blanks to the last column, and an inverted range (`from >= to`), an
    /// out-of-bounds `row`, or an empty grid changes nothing.
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
    /// in `fill` style (background-color erase). The row's end resets to
    /// [`RowEnd::Hard`].
    pub fn insert_cells(&mut self, row: u16, col: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if row >= rows || col >= cols {
            return;
        }

        let r = &mut self.rows[row as usize];
        let inserted = min(cols - col, n);

        r.truncate((cols - inserted) as usize);
        r.splice(
            col as usize..col as usize,
            std::iter::repeat_n(Cell::blank_with(fill), inserted as usize),
        );
        self.set_row_end(row, RowEnd::Hard);
    }

    /// Delete `n` cells starting at column `col` of `row`, shifting existing
    /// cells to the left; the freed space on the right is filled with blank cells
    /// in `fill` style (background-color erase). If `row` or `col` are out of
    /// bounds, this is a no-op. The row's end resets to [`RowEnd::Hard`].
    pub fn delete_cells(&mut self, row: u16, col: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if row >= rows || col >= cols {
            return;
        }

        let r = &mut self.rows[row as usize];
        let del = min(cols - col, n);

        r.drain(col as usize..(col + del) as usize);
        r.resize(cols as usize, Cell::blank_with(fill));
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

        // Never remove more lines than the band holds.
        let remove_count = min(n, last - first + 1);

        // Each iteration removes the band's top line — the lines below it slide
        // up — blanks that line to `cols` cells in place, and re-inserts it at
        // the band's bottom. Row metadata travels with each row.
        for _ in 0..remove_count as usize {
            let mut recycled = self.rows.remove(first as usize);
            recycled.clear();
            recycled.resize(cols as usize, Cell::blank_with(fill));
            self.rows.insert(last as usize, recycled);
            self.row_meta.remove(first as usize);
            self.row_meta.insert(last as usize, RowMeta::default());
        }
        if remove_count > 0 {
            // The row above the band and the row at `last - remove_count` both
            // end hard: each precedes a row it never wrapped into.
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
    /// outside the grid are no-ops, and an `n` of `0` leaves every row and
    /// every row end as it was.
    pub fn insert_lines(&mut self, first: u16, last: u16, n: u16, fill: Style) {
        let (rows, cols) = self.dimensions();
        if first >= rows || last >= rows || first > last {
            return;
        }

        // Never insert more lines than the band can hold.
        let insert_count = min(n, last - first + 1);

        // Each iteration removes the band's bottom line, blanks it to `cols`
        // cells in place, and re-inserts it at the band's top — the lines
        // between slide down. Row metadata travels with each row.
        for _ in 0..insert_count as usize {
            let mut recycled = self.rows.remove(last as usize);
            recycled.clear();
            recycled.resize(cols as usize, Cell::blank_with(fill));
            self.rows.insert(first as usize, recycled);
            self.row_meta.remove(last as usize);
            self.row_meta.insert(first as usize, RowMeta::default());
        }
        // The row above the band and the band's bottom row both end hard:
        // each precedes a row it never wrapped into.
        if insert_count > 0 {
            if first > 0 {
                self.set_row_end(first - 1, RowEnd::Hard);
            }
            self.set_row_end(last, RowEnd::Hard);
        }
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
        let cols = fields.rows.first().map_or(0, Vec::len);
        if fields.rows.iter().any(|row| row.len() != cols) {
            return Err(de::Error::custom("grid rows differ in length"));
        }
        Ok(Grid {
            rows: fields.rows,
            row_meta,
        })
    }
}

#[cfg(test)]
mod tests;
