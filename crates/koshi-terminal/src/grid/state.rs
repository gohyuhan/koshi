//! The cell grid: the 2-D array of [`Cell`]s backing one screen buffer.

use std::cmp::min;

use serde::de::{self, Deserializer as DeserializerTrait};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::style::{Color, Style};

/// A cell-sized portion of one retained native image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImageCellFragment {
    #[serde(rename = "source")]
    pub(crate) image_source_id: u64,
    #[serde(rename = "row")]
    pub(crate) source_row_index: u16,
    #[serde(rename = "column")]
    pub(crate) source_column_index: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum ImageFragments {
    #[default]
    Empty,
    One(ImageCellFragment),
    Many(Vec<ImageCellFragment>),
}

impl ImageFragments {
    fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }

    fn get_fragment_slice(&self) -> &[ImageCellFragment] {
        match self {
            Self::Empty => &[],
            Self::One(fragment) => std::slice::from_ref(fragment),
            Self::Many(fragments) => fragments,
        }
    }

    fn replace_fragment_by_image_source_id(&mut self, fragment: ImageCellFragment) -> bool {
        match self {
            Self::One(existing) if existing.image_source_id == fragment.image_source_id => {
                *existing = fragment;
                true
            }
            Self::Many(fragments) => {
                if let Some(existing) = fragments
                    .iter_mut()
                    .find(|existing| existing.image_source_id == fragment.image_source_id)
                {
                    *existing = fragment;
                    return true;
                }
                false
            }
            Self::Empty | Self::One(_) => false,
        }
    }

    fn append_image_fragment(&mut self, fragment: ImageCellFragment) {
        match self {
            Self::Empty => *self = Self::One(fragment),
            Self::One(existing) => *self = Self::Many(vec![*existing, fragment]),
            Self::Many(fragments) => fragments.push(fragment),
        }
    }

    fn clear_image_fragments(&mut self) {
        *self = Self::Empty;
    }

    fn compute_image_fragment_storage_byte_count(&self) -> usize {
        match self {
            Self::Many(fragments) => {
                fragments.capacity() * std::mem::size_of::<ImageCellFragment>()
            }
            Self::Empty | Self::One(_) => 0,
        }
    }
}

impl Serialize for ImageFragments {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_seq(self.get_fragment_slice())
    }
}

impl<'de> Deserialize<'de> for ImageFragments {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        struct ImageFragmentsVisitor;

        impl<'de> de::Visitor<'de> for ImageFragmentsVisitor {
            type Value = ImageFragments;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("bounded image cell fragments")
            }

            fn visit_seq<SequenceAccess>(
                self,
                mut sequence: SequenceAccess,
            ) -> Result<Self::Value, SequenceAccess::Error>
            where
                SequenceAccess: de::SeqAccess<'de>,
            {
                let mut image_fragments = Vec::new();
                while let Some(image_fragment) = sequence.next_element::<ImageCellFragment>()? {
                    if image_fragments.len() == crate::state::images::MAX_IMAGE_PLACEMENT_COUNT {
                        return Err(de::Error::custom("too many image fragments in one cell"));
                    }
                    if image_fragments
                        .iter()
                        .any(|candidate_fragment: &ImageCellFragment| {
                            candidate_fragment.image_source_id == image_fragment.image_source_id
                        })
                    {
                        return Err(de::Error::custom("duplicate image source in one cell"));
                    }
                    image_fragments.push(image_fragment);
                }
                match image_fragments.len() {
                    0 => Ok(ImageFragments::Empty),
                    1 => Ok(ImageFragments::One(image_fragments[0])),
                    _ => Ok(ImageFragments::Many(image_fragments)),
                }
            }
        }

        deserializer.deserialize_seq(ImageFragmentsVisitor)
    }
}

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
    /// Native image portions attached to this cell, in paint order.
    #[serde(default, skip_serializing_if = "ImageFragments::is_empty")]
    image_fragments: ImageFragments,
}

/// The image identity and source cell encoded by a Kitty Unicode placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImagePlaceholder {
    /// The low 24 bits encoded by the cell foreground color.
    pub(crate) image_id: u32,
    /// The optional placement id encoded by the underline color.
    pub(crate) placement_id: Option<u32>,
    /// The source row encoded by the first placeholder diacritic.
    #[serde(rename = "row")]
    pub(crate) source_row: Option<u16>,
    /// The source column encoded by the second placeholder diacritic.
    #[serde(rename = "column")]
    pub(crate) source_column: Option<u16>,
    /// The most significant image-id byte encoded by the third diacritic.
    pub(crate) image_id_msb: Option<u8>,
}

impl ImagePlaceholder {
    /// Build placeholder metadata from Kitty's foreground and underline colors.
    pub(crate) fn from_placeholder_style(style: Style) -> Self {
        Self {
            image_id: get_color_value(style.get_foreground_color()).unwrap_or_default(),
            placement_id: style
                .get_underline_color()
                .and_then(get_color_value)
                .filter(|placement_color_value| *placement_color_value != 0),
            source_row: None,
            source_column: None,
            image_id_msb: None,
        }
    }
}

fn get_color_value(color: Color) -> Option<u32> {
    match color {
        Color::Default => None,
        Color::Indexed(color_index) => Some(u32::from(color_index)),
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
    #[serde(rename = "ch")]
    character: char,
    /// The rest of the grapheme cluster layered over the base character
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
    #[serde(rename = "width")]
    display_width: u8,
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
            character: ' ',
            combining: None,
            display_width: 1,
            style,
        }
    }

    /// A cell holding `character` of the given `display_width`, in `style`.
    pub fn from_character(character: char, display_width: u8, style: Style) -> Self {
        Cell {
            character,
            combining: None,
            display_width,
            style,
        }
    }

    /// The character occupying this cell.
    pub fn get_character(&self) -> char {
        self.character
    }

    /// The rest of the grapheme cluster layered over the base character, in
    /// arrival order (combining marks plus any emoji continuation); empty for a
    /// plain cell.
    pub fn list_combining_characters(&self) -> &[char] {
        match &self.combining {
            Some(extra) => &extra.combining,
            None => &[],
        }
    }

    /// Return native image portions in paint order.
    pub(crate) fn image_fragments(&self) -> &[ImageCellFragment] {
        self.combining
            .as_ref()
            .map_or(&[], |extra| extra.image_fragments.get_fragment_slice())
    }

    /// Attach a native image portion, replacing or overlaying existing portions.
    pub(crate) fn set_image_fragment(&mut self, fragment: ImageCellFragment, should_overlay: bool) {
        if !should_overlay {
            *self = Self::blank_with(self.style);
        } else if self.combining.as_mut().is_some_and(|extra| {
            extra
                .image_fragments
                .replace_fragment_by_image_source_id(fragment)
        }) {
            return;
        }
        let cell_extra = self.combining.get_or_insert_with(|| {
            Box::new(CellExtra {
                combining: Vec::new(),
                image_placeholder: None,
                image_fragments: ImageFragments::Empty,
            })
        });
        cell_extra.image_fragments.append_image_fragment(fragment);
    }

    /// Heap bytes occupied by native image metadata in this cell.
    pub(crate) fn image_fragment_storage_bytes(&self) -> usize {
        self.combining
            .as_ref()
            .filter(|extra| !extra.image_fragments.is_empty())
            .map_or(0, |extra| {
                usize::from(extra.combining.is_empty() && extra.image_placeholder.is_none())
                    * std::mem::size_of::<CellExtra>()
                    + extra
                        .image_fragments
                        .compute_image_fragment_storage_byte_count()
            })
    }

    #[cfg(test)]
    pub(crate) fn get_image_fragment_capacity(&self) -> usize {
        self.combining.as_ref().map_or(0, |extra| {
            if let ImageFragments::Many(fragments) = &extra.image_fragments {
                fragments.capacity()
            } else {
                0
            }
        })
    }

    /// Remove native image portions from this cell.
    pub(crate) fn clear_image_fragments(&mut self) {
        if let Some(extra) = self.combining.as_mut() {
            extra.image_fragments.clear_image_fragments();
            if extra.combining.is_empty() && extra.image_placeholder.is_none() {
                self.combining = None;
            }
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
        self.character = ' ';
        self.combining
            .get_or_insert_with(|| {
                Box::new(CellExtra {
                    combining: Vec::new(),
                    image_placeholder: None,
                    image_fragments: ImageFragments::Empty,
                })
            })
            .image_placeholder = Some(placeholder);
    }

    /// Add one Kitty placeholder diacritic to the matching metadata slot.
    pub(crate) fn set_image_placeholder_diacritic(&mut self, placeholder_diacritic: char) -> bool {
        let Some(extra) = self.combining.as_mut() else {
            return false;
        };
        let Some(placeholder) = extra.image_placeholder.as_mut() else {
            return false;
        };
        let Some(diacritic_index) = find_image_placeholder_diacritic_index(placeholder_diacritic)
        else {
            return false;
        };
        if placeholder.source_row.is_none() {
            placeholder.source_row = Some(diacritic_index);
        } else if placeholder.source_column.is_none() {
            placeholder.source_column = Some(diacritic_index);
        } else if placeholder.image_id_msb.is_none() {
            placeholder.image_id_msb = u8::try_from(diacritic_index).ok();
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
                    image_fragments: ImageFragments::Empty,
                })
            })
            .combining
            .push(mark);
    }

    /// The cell's display width: 0 (combining/continuation), 1 (narrow), or 2
    /// (wide).
    pub fn get_display_width(&self) -> u8 {
        self.display_width
    }

    /// The cell's visual style.
    pub fn get_style(&self) -> Style {
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

fn find_image_placeholder_diacritic_index(mark: char) -> Option<u16> {
    IMAGE_PLACEHOLDER_DIACRITICS
        .iter()
        .position(|candidate| *candidate == mark)
        .and_then(|diacritic_index| u16::try_from(diacritic_index).ok())
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
pub struct RowMetadata {
    /// How the row ends relative to the row below it.
    #[serde(rename = "end")]
    pub row_end: RowEnd,
    /// Whether a shell reported a prompt on this row with OSC 133;A.
    #[serde(rename = "prompt")]
    pub has_prompt_mark: bool,
}

/// The number of content cells in a hard-ended row: its length with the
/// trailing run of fully-default blanks (the padding every row is filled
/// with) excluded. A styled blank — e.g. a background-colored prompt
/// segment — counts as content.
///
/// Only meaningful for a [`RowEnd::Hard`] row. A [`RowEnd::Soft`] row is full
/// of content, and a [`RowEnd::SoftWide`] row's final blank is a spacer
/// standing in for the wide glyph on the next row.
pub(crate) fn count_row_content_cells(row_cells: &[Cell]) -> usize {
    let blank = Cell::blank();
    row_cells
        .iter()
        .rposition(|cell| *cell != blank)
        .map_or(0, |content_cell_index| content_cell_index + 1)
}

/// A fixed-size grid of cells, addressed by row and column indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grid {
    /// Row-major cell storage: `rows[row][col]`.
    rows: Vec<Vec<Cell>>,
    /// Per-row metadata, parallel to `rows`. Every operation that adds,
    /// removes, or reorders rows maintains it.
    #[serde(rename = "row_meta")]
    row_metadata: Vec<RowMetadata>,
}

impl Grid {
    /// Build a grid with `row_count` rows and `column_count` columns, filling
    /// every cell with a blank space in `fill_style`.
    pub fn blank(row_count: u16, column_count: u16, fill_style: Style) -> Self {
        Grid {
            rows: vec![
                vec![Cell::blank_with(fill_style); column_count as usize];
                row_count as usize
            ],
            row_metadata: vec![RowMetadata::default(); row_count as usize],
        }
    }

    /// Build a grid from ready-made row cells, normalizing each row to exactly
    /// `column_count` cells. Every row starts with default metadata.
    pub fn from_rows(row_cells: Vec<Vec<Cell>>, column_count: u16, fill_style: Style) -> Self {
        let row_cells = row_cells
            .into_iter()
            .map(|row_cells| (row_cells, RowMetadata::default()))
            .collect();
        Self::from_rows_with_metadata(row_cells, column_count, fill_style)
    }

    /// Build a grid from rows and their metadata, normalizing every row to
    /// exactly `column_count` cells.
    pub(crate) fn from_rows_with_metadata(
        mut row_cells: Vec<(Vec<Cell>, RowMetadata)>,
        column_count: u16,
        fill_style: Style,
    ) -> Self {
        for (row_cells, _) in &mut row_cells {
            row_cells.resize(column_count as usize, Cell::blank_with(fill_style));
        }
        let (rows, row_metadata): (Vec<Vec<Cell>>, Vec<RowMetadata>) =
            row_cells.into_iter().unzip();
        Grid { rows, row_metadata }
    }

    /// Everything recorded about `row_index` apart from its cells; out of bounds
    /// reads as [`RowMetadata::default`] — a [`RowEnd::Hard`] end and no prompt
    /// mark.
    pub fn get_row_metadata(&self, row_index: u16) -> RowMetadata {
        self.row_metadata
            .get(row_index as usize)
            .copied()
            .unwrap_or_default()
    }

    /// How `row_index` ends relative to the row below it; out of bounds reads as
    /// [`RowEnd::Hard`].
    pub fn get_row_end(&self, row_index: u16) -> RowEnd {
        self.get_row_metadata(row_index).row_end
    }

    /// Record how `row_index` ends relative to the row below it. Out of bounds is a
    /// no-op.
    pub fn set_row_end(&mut self, row_index: u16, row_end: RowEnd) {
        if let Some(row_metadata) = self.row_metadata.get_mut(row_index as usize) {
            row_metadata.row_end = row_end;
        }
    }

    /// Whether a shell reported a prompt on `row_index`; out of bounds reads false.
    pub fn has_prompt_mark(&self, row_index: u16) -> bool {
        self.get_row_metadata(row_index).has_prompt_mark
    }

    /// Set whether a shell reported a prompt on `row_index`. Out of bounds is a
    /// no-op.
    pub fn set_prompt_mark(&mut self, row_index: u16, has_prompt_mark: bool) {
        if let Some(row_metadata) = self.row_metadata.get_mut(row_index as usize) {
            row_metadata.has_prompt_mark = has_prompt_mark;
        }
    }

    /// The grid's dimensions as `(row_count, column_count)`.
    pub fn get_grid_dimensions(&self) -> (u16, u16) {
        (
            self.rows.len() as u16,
            self.rows.first().map_or(0, Vec::len) as u16,
        )
    }

    /// A reference to the cell at (`row_index`, `column_index`), or `None` if
    /// out of bounds.
    pub fn get_cell(&self, row_index: u16, column_index: u16) -> Option<&Cell> {
        self.rows
            .get(row_index as usize)?
            .get(column_index as usize)
    }

    /// A mutable reference to the cell at (`row_index`, `column_index`), or
    /// `None` if out of bounds.
    pub fn get_cell_mut(&mut self, row_index: u16, column_index: u16) -> Option<&mut Cell> {
        self.rows
            .get_mut(row_index as usize)?
            .get_mut(column_index as usize)
    }

    /// All rows, row-major.
    pub fn list_rows(&self) -> &[Vec<Cell>] {
        &self.rows
    }

    /// Blank the other half of a wide glyph overwritten at this cell.
    pub(crate) fn clear_wide_glyph_at(
        &mut self,
        row_index: u16,
        column_index: u16,
        fill_style: Style,
    ) {
        let paired_column_index = match self
            .get_cell(row_index, column_index)
            .map_or(1, Cell::get_display_width)
        {
            2 => column_index.checked_add(1),
            0 => column_index.checked_sub(1),
            _ => None,
        };
        if let Some(paired_column_index) = paired_column_index {
            if let Some(cell) = self.get_cell_mut(row_index, paired_column_index) {
                *cell = Cell::blank_with(fill_style);
            }
        }
    }

    /// Blank columns `first_column_index..last_column_index_exclusive` in `row_index`.
    /// The row end resets to [`RowEnd::Hard`] when the span reaches the right
    /// edge. Out-of-bounds indexes and an empty span change nothing.
    pub fn clear_line(
        &mut self,
        row_index: u16,
        first_column_index: u16,
        last_column_index_exclusive: u16,
        fill_style: Style,
    ) {
        if let Some(row_cells) = self.rows.get_mut(row_index as usize) {
            let end_column_index = (last_column_index_exclusive as usize).min(row_cells.len());
            if let Some(cell_span) =
                row_cells.get_mut(first_column_index as usize..end_column_index)
            {
                cell_span.fill(Cell::blank_with(fill_style));
            }
        }
        let (_, column_count) = self.get_grid_dimensions();
        if last_column_index_exclusive >= column_count && first_column_index < column_count {
            self.set_row_end(row_index, RowEnd::Hard);
        }
    }

    /// Insert `insert_cell_count` blank cells at `column_index` in `row_index`.
    /// Cells pushed past the right edge are dropped. The row end resets to
    /// [`RowEnd::Hard`].
    pub fn insert_cells(
        &mut self,
        row_index: u16,
        column_index: u16,
        insert_cell_count: u16,
        fill_style: Style,
    ) {
        let (row_count, column_count) = self.get_grid_dimensions();
        if row_index >= row_count || column_index >= column_count {
            return;
        }

        let row_cells = &mut self.rows[row_index as usize];
        let inserted_cell_count = min(column_count - column_index, insert_cell_count);

        row_cells.truncate((column_count - inserted_cell_count) as usize);
        row_cells.splice(
            column_index as usize..column_index as usize,
            std::iter::repeat_n(Cell::blank_with(fill_style), inserted_cell_count as usize),
        );
        self.set_row_end(row_index, RowEnd::Hard);
    }

    /// Delete `delete_cell_count` cells starting at `column_index` in `row_index`.
    /// Freed space on the right is filled with blank cells. The row end resets
    /// to [`RowEnd::Hard`].
    pub fn delete_cells(
        &mut self,
        row_index: u16,
        column_index: u16,
        delete_cell_count: u16,
        fill_style: Style,
    ) {
        let (row_count, column_count) = self.get_grid_dimensions();
        if row_index >= row_count || column_index >= column_count {
            return;
        }

        let row_cells = &mut self.rows[row_index as usize];
        let deleted_cell_count = min(column_count - column_index, delete_cell_count);

        row_cells.drain(column_index as usize..(column_index + deleted_cell_count) as usize);
        row_cells.resize(column_count as usize, Cell::blank_with(fill_style));
        self.set_row_end(row_index, RowEnd::Hard);
    }

    /// Delete `delete_row_count` lines from the band
    /// `[first_row_index, last_row_index]` (both inclusive), shifting
    /// lines below the band upward; blank lines are inserted at the bottom of the
    /// band to preserve the band's height. Cells are filled in `fill_style`
    /// (background-color erase). Coordinates outside the grid are no-ops.
    pub fn delete_lines(
        &mut self,
        first_row_index: u16,
        last_row_index: u16,
        delete_row_count: u16,
        fill_style: Style,
    ) {
        let (row_count, column_count) = self.get_grid_dimensions();
        if first_row_index >= row_count
            || last_row_index >= row_count
            || first_row_index > last_row_index
        {
            return;
        }

        // Never remove more lines than the band holds.
        let removed_line_count = min(delete_row_count, last_row_index - first_row_index + 1);

        // Each iteration removes the band's top line — the lines below it slide
        // up — blanks that line to `column_count` cells in place, and re-inserts it at
        // the band's bottom. Row metadata travels with each row.
        for _ in 0..removed_line_count as usize {
            let mut recycled_row_cells = self.rows.remove(first_row_index as usize);
            recycled_row_cells.clear();
            recycled_row_cells.resize(column_count as usize, Cell::blank_with(fill_style));
            self.rows
                .insert(last_row_index as usize, recycled_row_cells);
            self.row_metadata.remove(first_row_index as usize);
            self.row_metadata
                .insert(last_row_index as usize, RowMetadata::default());
        }
        if removed_line_count > 0 {
            // The row above the band and the row at `last_row - removed_line_count` both
            // end hard: each precedes a row it never wrapped into.
            if first_row_index > 0 {
                self.set_row_end(first_row_index - 1, RowEnd::Hard);
            }
            if let Some(slid_last) = last_row_index.checked_sub(removed_line_count) {
                self.set_row_end(slid_last, RowEnd::Hard);
            }
        }
    }

    /// Insert `insert_row_count` blank lines within the band
    /// `[first_row_index, last_row_index]`
    /// (both inclusive), shifting lines downward. Lines pushed below the band
    /// are dropped. Blank lines use `fill_style`. An `insert_row_count` of `0`
    /// leaves every row and every row end unchanged.
    pub fn insert_lines(
        &mut self,
        first_row_index: u16,
        last_row_index: u16,
        insert_row_count: u16,
        fill_style: Style,
    ) {
        let (row_count, column_count) = self.get_grid_dimensions();
        if first_row_index >= row_count
            || last_row_index >= row_count
            || first_row_index > last_row_index
        {
            return;
        }

        // Never insert more lines than the band can hold.
        let inserted_line_count = min(insert_row_count, last_row_index - first_row_index + 1);

        // Each iteration removes the band's bottom line, blanks it to `column_count`
        // cells in place, and re-inserts it at the band's top — the lines
        // between slide down. Row metadata travels with each row.
        for _ in 0..inserted_line_count as usize {
            let mut recycled_row_cells = self.rows.remove(last_row_index as usize);
            recycled_row_cells.clear();
            recycled_row_cells.resize(column_count as usize, Cell::blank_with(fill_style));
            self.rows
                .insert(first_row_index as usize, recycled_row_cells);
            self.row_metadata.remove(last_row_index as usize);
            self.row_metadata
                .insert(first_row_index as usize, RowMetadata::default());
        }
        // The row above the band and the band's bottom row both end hard:
        // each precedes a row it never wrapped into.
        if inserted_line_count > 0 {
            if first_row_index > 0 {
                self.set_row_end(first_row_index - 1, RowEnd::Hard);
            }
            self.set_row_end(last_row_index, RowEnd::Hard);
        }
    }
}

#[derive(Deserialize)]
struct GridFields {
    rows: Vec<Vec<Cell>>,
    #[serde(rename = "row_meta", default)]
    row_metadata: Option<Vec<RowMetadata>>,
    #[serde(default)]
    row_ends: Option<Vec<RowEnd>>,
}

impl<'de> Deserialize<'de> for Grid {
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        let grid_fields = GridFields::deserialize(deserializer)?;
        let row_metadata = match (grid_fields.row_metadata, grid_fields.row_ends) {
            (Some(row_metadata), _) => row_metadata,
            (None, Some(row_ends)) => row_ends
                .into_iter()
                .map(|row_end| RowMetadata {
                    row_end,
                    has_prompt_mark: false,
                })
                .collect(),
            (None, None) => vec![RowMetadata::default(); grid_fields.rows.len()],
        };
        if row_metadata.len() != grid_fields.rows.len() {
            return Err(de::Error::custom("grid row metadata does not match rows"));
        }
        let column_count = grid_fields.rows.first().map_or(0, Vec::len);
        if grid_fields
            .rows
            .iter()
            .any(|row_cells| row_cells.len() != column_count)
        {
            return Err(de::Error::custom("grid rows differ in length"));
        }
        Ok(Grid {
            rows: grid_fields.rows,
            row_metadata,
        })
    }
}

#[cfg(test)]
mod tests;
