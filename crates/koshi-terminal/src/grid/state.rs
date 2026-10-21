//! The cell grid: the 2-D array of [`Cell`]s backing one screen buffer.

use std::cmp::min;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use serde::de::{self, Deserializer as DeserializerTrait};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::grid::row_snapshot::RowSnapshot;
use crate::style::{Color, Style};

/// Upper bound on the continuation code points one cell keeps in its
/// `combining` tail. A continuation that arrives when the base already holds
/// this many is dropped. A real grapheme cluster — a skin-toned ZWJ
/// (zero-width joiner) emoji family included — stays well under it; a flood of
/// combining marks ("zalgo" text) reaches it.
pub(crate) const MAX_GRAPHEME_CONTINUATION_COUNT: usize = 32;

/// A cell-sized portion of one retained native image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImageCellFragment {
    pub(crate) image_source_id: u64,
    pub(crate) source_row_index: u16,
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

/// The part of a cell that almost no cell has: continuation code points,
/// Kitty placeholder metadata, and native image fragments. A [`Cell`] holds
/// it behind one pointer, eight bytes on a 64-bit target, null for ordinary
/// cells.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CellExtra {
    /// The continuation code points in arrival order.
    combining: Vec<char>,
    /// Kitty Unicode-placeholder metadata, when the base cell is a placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image_placeholder: Option<ImagePlaceholder>,
    /// Native image portions attached to this cell, in paint order.
    #[serde(default, skip_serializing_if = "ImageFragments::is_empty")]
    image_fragments: ImageFragments,
}

impl CellExtra {
    /// Whether this tail carries a Kitty placeholder or a native image
    /// fragment.
    fn has_image_data(&self) -> bool {
        self.image_placeholder.is_some() || !self.image_fragments.is_empty()
    }

    /// The Kitty Unicode-placeholder metadata, when the base cell is a
    /// placeholder.
    pub(crate) fn get_image_placeholder(&self) -> Option<ImagePlaceholder> {
        self.image_placeholder
    }

    /// The native image portions, in paint order.
    pub(crate) fn get_image_fragments(&self) -> &[ImageCellFragment] {
        self.image_fragments.get_fragment_slice()
    }

    /// Heap bytes this tail spends on native image fragments: the tail itself
    /// when it holds nothing but fragments, plus the fragment list past its
    /// first entry. `0` with no fragment.
    pub(crate) fn compute_image_fragment_storage_bytes(&self) -> usize {
        if self.image_fragments.is_empty() {
            return 0;
        }
        usize::from(self.combining.is_empty() && self.image_placeholder.is_none())
            * std::mem::size_of::<CellExtra>()
            + self
                .image_fragments
                .compute_image_fragment_storage_byte_count()
    }
}

/// The image identity and source cell encoded by a Kitty Unicode placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImagePlaceholder {
    /// The low 24 bits encoded by the cell foreground color.
    pub(crate) image_id: u32,
    /// The optional placement id encoded by the underline color.
    pub(crate) placement_id: Option<u32>,
    /// The source row encoded by the first placeholder diacritic.
    pub(crate) source_row: Option<u16>,
    /// The source column encoded by the second placeholder diacritic.
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
    character: char,
    /// The rest of the grapheme cluster layered over the base character
    /// — a grapheme cluster is the run of code points a person perceives as
    /// one visual character — in arrival order: combining accents, variation
    /// selectors, and the joined parts of a multi-codepoint emoji (ZWJ-joined
    /// glyphs, skin-tone modifiers, the second half of a flag). `None` for a
    /// plain cell; the renderer draws `character` followed by these as one
    /// glyph.
    ///
    /// [`push_combining`](Cell::push_combining) is the normal writer; a
    /// placeholder can allocate the same storage without continuation marks.
    combining: Option<Box<CellExtra>>,
    /// Display width in cells: 0 (continuation half of a wide glyph), 1
    /// (narrow), or 2 (wide, e.g. CJK).
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
/// in one of [`AttributeFlags`](crate::style::AttributeFlags)'s spare bits. Raising the
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
    pub fn build_blank() -> Self {
        Cell::build_blank_with_style(Style::default())
    }

    /// A blank cell — a single space — in the given `style`. Erased and
    /// scrolled cells are built this way with the pen's background
    /// (background-color erase); the pen is the color and attribute state
    /// applied to newly written text.
    pub fn build_blank_with_style(style: Style) -> Self {
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

    /// The continuation tail of this cell when it carries a Kitty placeholder
    /// or a native image fragment, and `None` otherwise.
    pub(crate) fn get_image_cell_extra(&self) -> Option<&CellExtra> {
        self.combining
            .as_deref()
            .filter(|cell_extra| cell_extra.has_image_data())
    }

    /// Replace this cell's continuation tail with `cell_extra`.
    pub(crate) fn set_cell_extra(&mut self, cell_extra: CellExtra) {
        self.combining = Some(Box::new(cell_extra));
    }

    /// Return native image portions in paint order.
    pub(crate) fn get_image_fragments(&self) -> &[ImageCellFragment] {
        self.combining
            .as_ref()
            .map_or(&[], |extra| extra.image_fragments.get_fragment_slice())
    }

    /// Attach a native image portion, replacing or overlaying existing portions.
    pub(crate) fn set_image_fragment(&mut self, fragment: ImageCellFragment, should_overlay: bool) {
        if !should_overlay {
            *self = Self::build_blank_with_style(self.style);
        } else if self.combining.as_mut().is_some_and(|extra| {
            extra
                .image_fragments
                .replace_fragment_by_image_source_id(fragment)
        }) {
            return;
        }
        let cell_extra = self.combining.get_or_insert_with(Box::default);
        cell_extra.image_fragments.append_image_fragment(fragment);
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
    pub(crate) fn get_image_placeholder(&self) -> Option<ImagePlaceholder> {
        self.combining
            .as_ref()
            .and_then(|extra| extra.image_placeholder)
    }

    /// Return whether this cell carries a Kitty Unicode-placeholder marker.
    #[must_use]
    pub fn has_image_placeholder(&self) -> bool {
        self.get_image_placeholder().is_some()
    }

    /// Set the Kitty Unicode-placeholder metadata and use a blank base glyph.
    pub(crate) fn set_image_placeholder(&mut self, placeholder: ImagePlaceholder) {
        self.character = ' ';
        self.combining
            .get_or_insert_with(Box::default)
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
            .get_or_insert_with(Box::default)
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
    pub row_end: RowEnd,
    /// Whether a shell reported a prompt on this row with OSC 133;A.
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
    let blank = Cell::build_blank();
    row_cells
        .iter()
        .rposition(|cell| *cell != blank)
        .map_or(0, |content_cell_index| content_cell_index + 1)
}

/// One row of a [`Grid`]: its cells, its metadata, and the snapshot the last
/// frame took of it.
#[derive(Debug, Clone)]
struct GridRow {
    /// The row's cells: exactly the grid's column count.
    cells: Vec<Cell>,
    /// How the row ends, and whether a shell marked a prompt on it.
    row_metadata: RowMetadata,
    /// Every cell at or past this column equals
    /// `Cell::build_blank_with_style(blank_fill_style)`.
    written_cell_count: usize,
    /// The style of the blanks at and past `written_cell_count`.
    blank_fill_style: Style,
    /// `Some(style)` when the row is plain text in one style: every cell a
    /// narrow ASCII character or blank in `style`, with no combining mark and
    /// no image data. `None` when the row may hold any other cell.
    plain_row_style: Option<Style>,
    /// The snapshot the last frame took of the row. Every change to the row's
    /// cells or metadata takes it away.
    published_row_snapshot: OnceLock<Arc<RowSnapshot>>,
}

impl GridRow {
    /// A row of `column_count` blanks in `fill_style`, with default metadata.
    fn from_blank(column_count: usize, fill_style: Style) -> Self {
        GridRow {
            cells: vec![Cell::build_blank_with_style(fill_style); column_count],
            row_metadata: RowMetadata::default(),
            written_cell_count: 0,
            blank_fill_style: fill_style,
            plain_row_style: Some(fill_style),
            published_row_snapshot: OnceLock::new(),
        }
    }

    /// A row holding `cells` with `row_metadata`. Every cell counts as written.
    fn from_cells(cells: Vec<Cell>, row_metadata: RowMetadata) -> Self {
        GridRow {
            written_cell_count: cells.len(),
            cells,
            row_metadata,
            blank_fill_style: Style::default(),
            plain_row_style: None,
            published_row_snapshot: OnceLock::new(),
        }
    }

    /// The cells in `column_range`, to change. The row's snapshot goes away,
    /// and the row no longer counts as plain text.
    ///
    /// # Panics
    /// Panics when `column_range` reaches past the row's end.
    fn get_cells_mut(&mut self, column_range: Range<usize>) -> &mut [Cell] {
        self.published_row_snapshot.take();
        self.plain_row_style = None;
        self.written_cell_count = self.written_cell_count.max(column_range.end);
        &mut self.cells[column_range]
    }

    /// Write one narrow cell in `style` per byte of `ascii_bytes`, printable
    /// ASCII characters, from `first_column_index` on. The row's snapshot goes
    /// away. The row stays plain text when it was plain text in `style`, and
    /// becomes plain text when the bytes cover it whole.
    ///
    /// # Panics
    /// Panics when the bytes reach past the row's end.
    fn write_ascii_cells(&mut self, first_column_index: usize, ascii_bytes: &[u8], style: Style) {
        let column_range = first_column_index..first_column_index + ascii_bytes.len();
        let is_whole_row = column_range == (0..self.cells.len());
        let plain_row_style =
            (self.plain_row_style == Some(style) || is_whole_row).then_some(style);
        let written_cells = self.get_cells_mut(column_range);
        for (written_cell, ascii_byte) in written_cells.iter_mut().zip(ascii_bytes) {
            *written_cell = Cell::from_character(char::from(*ascii_byte), 1, style);
        }
        self.plain_row_style = plain_row_style;
    }

    /// The row's metadata, to change. The row's snapshot goes away.
    fn get_row_metadata_mut(&mut self) -> &mut RowMetadata {
        self.published_row_snapshot.take();
        &mut self.row_metadata
    }

    /// Blank `column_range` in `fill_style`. `column_range` ends at or before
    /// the row's end. A span that reaches the row's end
    /// rewrites only the cells that can differ from that blank.
    fn fill_with_blank(&mut self, column_range: Range<usize>, fill_style: Style) {
        let blank_cell = Cell::build_blank_with_style(fill_style);
        let is_whole_row = column_range == (0..self.cells.len());
        let plain_row_style =
            (self.plain_row_style == Some(fill_style) || is_whole_row).then_some(fill_style);
        if column_range.end < self.cells.len() {
            self.get_cells_mut(column_range).fill(blank_cell);
            self.plain_row_style = plain_row_style;
            return;
        }
        self.published_row_snapshot.take();
        self.plain_row_style = plain_row_style;
        let first_column_index = column_range.start;
        let (rewritten_end_index, written_cell_count) = if fill_style == self.blank_fill_style {
            (
                self.written_cell_count.max(first_column_index),
                self.written_cell_count.min(first_column_index),
            )
        } else {
            (self.cells.len(), first_column_index)
        };
        self.cells[first_column_index..rewritten_end_index].fill(blank_cell);
        self.written_cell_count = written_cell_count;
        self.blank_fill_style = fill_style;
    }

    /// Blank the whole row in `fill_style` and give it default metadata.
    fn reset_to_blank(&mut self, fill_style: Style) {
        self.fill_with_blank(0..self.cells.len(), fill_style);
        self.row_metadata = RowMetadata::default();
    }

    /// The row's snapshot: the one the last frame took when nothing changed
    /// the row since, or a new one, which the row keeps.
    fn get_row_snapshot(&self) -> Arc<RowSnapshot> {
        Arc::clone(
            self.published_row_snapshot
                .get_or_init(|| Arc::new(self.encode_row_snapshot(Vec::new()))),
        )
    }

    /// The row's snapshot for history: the one the last frame took when
    /// nothing changed the row since, or a new one encoded into
    /// `encoded_row_bytes`. The row keeps no snapshot afterwards.
    fn take_row_snapshot(&mut self, encoded_row_bytes: Vec<u8>) -> Arc<RowSnapshot> {
        match self.published_row_snapshot.take() {
            Some(row_snapshot) => row_snapshot,
            None => Arc::new(self.encode_row_snapshot(encoded_row_bytes)),
        }
    }

    /// A new snapshot of the row's cells and metadata, encoded into
    /// `encoded_row_bytes`.
    fn encode_row_snapshot(&self, encoded_row_bytes: Vec<u8>) -> RowSnapshot {
        RowSnapshot::from_padded_row_cells(
            &self.cells,
            u16::try_from(self.cells.len()).expect("a row holds at most 65 535 cells"),
            self.plain_row_style,
            self.row_metadata,
            encoded_row_bytes,
        )
    }
}

impl PartialEq for GridRow {
    /// Two rows are equal when their cells and metadata are equal.
    fn eq(&self, other_grid_row: &Self) -> bool {
        self.cells == other_grid_row.cells && self.row_metadata == other_grid_row.row_metadata
    }
}

impl Eq for GridRow {}

/// A fixed-size grid of cells, addressed by row and column indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    /// The rows, top to bottom. Every operation that adds, removes, or
    /// reorders rows moves each row's metadata with its cells.
    rows: Vec<GridRow>,
}

impl Grid {
    /// Build a grid with `row_count` rows and `column_count` columns, filling
    /// every cell with a blank space in `fill_style`.
    pub fn build_blank(row_count: u16, column_count: u16, fill_style: Style) -> Self {
        Grid {
            rows: (0..row_count)
                .map(|_| GridRow::from_blank(usize::from(column_count), fill_style))
                .collect(),
        }
    }

    /// Build a grid from rows and their metadata, normalizing every row to
    /// exactly `column_count` cells.
    pub(crate) fn from_rows_with_metadata(
        row_cells: Vec<(Vec<Cell>, RowMetadata)>,
        column_count: u16,
        fill_style: Style,
    ) -> Self {
        Grid {
            rows: row_cells
                .into_iter()
                .map(|(mut row_cells, row_metadata)| {
                    row_cells.resize(
                        usize::from(column_count),
                        Cell::build_blank_with_style(fill_style),
                    );
                    GridRow::from_cells(row_cells, row_metadata)
                })
                .collect(),
        }
    }

    /// Everything recorded about `row_index` apart from its cells; out of bounds
    /// reads as [`RowMetadata::default`] — a [`RowEnd::Hard`] end and no prompt
    /// mark.
    pub fn get_row_metadata(&self, row_index: u16) -> RowMetadata {
        self.rows
            .get(row_index as usize)
            .map(|grid_row| grid_row.row_metadata)
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
        if let Some(grid_row) = self.rows.get_mut(row_index as usize) {
            if grid_row.row_metadata.row_end != row_end {
                grid_row.get_row_metadata_mut().row_end = row_end;
            }
        }
    }

    /// Whether a shell reported a prompt on `row_index`; out of bounds reads false.
    pub fn has_prompt_mark(&self, row_index: u16) -> bool {
        self.get_row_metadata(row_index).has_prompt_mark
    }

    /// Set whether a shell reported a prompt on `row_index`. Out of bounds is a
    /// no-op.
    pub fn set_prompt_mark(&mut self, row_index: u16, has_prompt_mark: bool) {
        if let Some(grid_row) = self.rows.get_mut(row_index as usize) {
            if grid_row.row_metadata.has_prompt_mark != has_prompt_mark {
                grid_row.get_row_metadata_mut().has_prompt_mark = has_prompt_mark;
            }
        }
    }

    /// The grid's dimensions as `(row_count, column_count)`.
    pub fn get_grid_dimensions(&self) -> (u16, u16) {
        (
            self.rows.len() as u16,
            self.rows.first().map_or(0, |grid_row| grid_row.cells.len()) as u16,
        )
    }

    /// A reference to the cell at (`row_index`, `column_index`), or `None` if
    /// out of bounds.
    pub fn get_cell(&self, row_index: u16, column_index: u16) -> Option<&Cell> {
        self.rows
            .get(row_index as usize)?
            .cells
            .get(column_index as usize)
    }

    /// A mutable reference to the cell at (`row_index`, `column_index`), or
    /// `None` if out of bounds.
    pub fn get_cell_mut(&mut self, row_index: u16, column_index: u16) -> Option<&mut Cell> {
        let grid_row = self.rows.get_mut(row_index as usize)?;
        let column_index = usize::from(column_index);
        if column_index >= grid_row.cells.len() {
            return None;
        }
        grid_row
            .get_cells_mut(column_index..column_index + 1)
            .first_mut()
    }

    /// The cells of `row_index`, or `None` past the last row.
    pub fn get_row_cells(&self, row_index: u16) -> Option<&[Cell]> {
        self.rows
            .get(row_index as usize)
            .map(|grid_row| grid_row.cells.as_slice())
    }

    /// Every row's cells, top to bottom.
    pub fn list_row_cells(&self) -> impl ExactSizeIterator<Item = &[Cell]> {
        self.rows.iter().map(|grid_row| grid_row.cells.as_slice())
    }

    /// The snapshot of `row_index`, or `None` past the last row. A row that
    /// nothing changed since its last snapshot gives that snapshot again;
    /// any other row is encoded once and keeps the new snapshot.
    pub fn get_row_snapshot(&self, row_index: u16) -> Option<Arc<RowSnapshot>> {
        self.rows
            .get(row_index as usize)
            .map(GridRow::get_row_snapshot)
    }

    /// Every row's snapshot, top to bottom, as
    /// [`get_row_snapshot`](Self::get_row_snapshot) gives each one.
    pub fn list_row_snapshots(&self) -> impl ExactSizeIterator<Item = Arc<RowSnapshot>> + '_ {
        self.rows.iter().map(GridRow::get_row_snapshot)
    }

    /// The snapshot of `row_index` for history, or `None` past the last row: the
    /// row's last snapshot when nothing changed the row since, or a new one
    /// encoded into `encoded_row_bytes`. The row keeps no snapshot afterwards.
    pub(crate) fn take_row_snapshot(
        &mut self,
        row_index: u16,
        encoded_row_bytes: Vec<u8>,
    ) -> Option<Arc<RowSnapshot>> {
        self.rows
            .get_mut(row_index as usize)
            .map(|grid_row| grid_row.take_row_snapshot(encoded_row_bytes))
    }

    /// Write one narrow cell in `style` per byte of `ascii_bytes`, printable
    /// ASCII characters, into `row_index` from `first_column_index` on. Bytes
    /// past the row's end and a row past the last are dropped.
    ///
    /// Example — `ab` at column 3 makes columns 3 and 4 hold `a` and `b`, each
    /// 1 column wide.
    pub(crate) fn write_ascii_cells(
        &mut self,
        row_index: u16,
        first_column_index: u16,
        ascii_bytes: &[u8],
        style: Style,
    ) {
        let Some(grid_row) = self.rows.get_mut(row_index as usize) else {
            return;
        };
        let first_column_index = usize::from(first_column_index);
        let written_byte_count = ascii_bytes
            .len()
            .min(grid_row.cells.len().saturating_sub(first_column_index));
        if written_byte_count == 0 {
            return;
        }
        grid_row.write_ascii_cells(
            first_column_index,
            &ascii_bytes[..written_byte_count],
            style,
        );
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
                *cell = Cell::build_blank_with_style(fill_style);
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
        if let Some(grid_row) = self.rows.get_mut(row_index as usize) {
            let end_column_index = (last_column_index_exclusive as usize).min(grid_row.cells.len());
            if (first_column_index as usize) < end_column_index {
                grid_row.fill_with_blank(first_column_index as usize..end_column_index, fill_style);
            }
        }
        let (_, column_count) = self.get_grid_dimensions();
        if last_column_index_exclusive >= column_count && first_column_index < column_count {
            self.set_row_end(row_index, RowEnd::Hard);
        }
    }

    /// Insert blank cells within the inclusive column range. Cells pushed past
    /// `last_column_index` are dropped, and the row end resets to hard.
    pub(crate) fn insert_cells_in_columns(
        &mut self,
        row_index: u16,
        first_column_index: u16,
        last_column_index: u16,
        insert_cell_count: u16,
        fill_style: Style,
    ) {
        let Some(grid_row) = self.rows.get_mut(row_index as usize) else {
            return;
        };
        let first_column_index = first_column_index as usize;
        let last_column_index = (last_column_index as usize + 1).min(grid_row.cells.len());
        if first_column_index >= last_column_index {
            return;
        }

        let column_span = last_column_index - first_column_index;
        let inserted_cell_count = column_span.min(insert_cell_count as usize);
        let span_cells = grid_row.get_cells_mut(first_column_index..last_column_index);
        span_cells.rotate_right(inserted_cell_count);
        span_cells[..inserted_cell_count].fill(Cell::build_blank_with_style(fill_style));
        self.set_row_end(row_index, RowEnd::Hard);
    }

    /// Delete cells within the inclusive column range. Freed cells at the
    /// range's right edge are filled with `fill_style`, and the row end resets
    /// to hard.
    pub(crate) fn delete_cells_in_columns(
        &mut self,
        row_index: u16,
        first_column_index: u16,
        last_column_index: u16,
        delete_cell_count: u16,
        fill_style: Style,
    ) {
        let Some(grid_row) = self.rows.get_mut(row_index as usize) else {
            return;
        };
        let first_column_index = first_column_index as usize;
        let last_column_index = (last_column_index as usize + 1).min(grid_row.cells.len());
        if first_column_index >= last_column_index {
            return;
        }

        let column_span = last_column_index - first_column_index;
        let deleted_cell_count = column_span.min(delete_cell_count as usize);
        let span_cells = grid_row.get_cells_mut(first_column_index..last_column_index);
        span_cells.rotate_left(deleted_cell_count);
        span_cells[column_span - deleted_cell_count..]
            .fill(Cell::build_blank_with_style(fill_style));
        self.set_row_end(row_index, RowEnd::Hard);
    }

    /// Delete lines while moving only the inclusive column range. Each row
    /// keeps its row end and prompt mark. A `delete_row_count` of `0` leaves
    /// every cell unchanged.
    pub(crate) fn delete_lines_in_columns(
        &mut self,
        first_row_index: u16,
        last_row_index: u16,
        delete_row_count: u16,
        first_column_index: u16,
        last_column_index: u16,
        fill_style: Style,
    ) {
        let (row_count, column_count) = self.get_grid_dimensions();
        if first_row_index >= row_count
            || last_row_index >= row_count
            || first_row_index > last_row_index
            || column_count == 0
        {
            return;
        }
        let first_column_index = first_column_index.min(column_count - 1) as usize;
        let last_column_index = (last_column_index.min(column_count - 1) as usize) + 1;
        if first_column_index >= last_column_index {
            return;
        }

        let removed_row_count =
            min(delete_row_count, last_row_index - first_row_index + 1) as usize;
        if removed_row_count == 0 {
            return;
        }
        let band_rows = &mut self.rows[usize::from(first_row_index)..=usize::from(last_row_index)];
        let band_row_count = band_rows.len();
        for source_row_index in removed_row_count..band_row_count {
            let (upper_rows, lower_rows) = band_rows.split_at_mut(source_row_index);
            upper_rows[source_row_index - removed_row_count]
                .get_cells_mut(first_column_index..last_column_index)
                .swap_with_slice(
                    lower_rows[0].get_cells_mut(first_column_index..last_column_index),
                );
        }
        for vacated_row in &mut band_rows[band_row_count - removed_row_count..] {
            vacated_row
                .get_cells_mut(first_column_index..last_column_index)
                .fill(Cell::build_blank_with_style(fill_style));
        }
    }

    /// Insert lines while moving only the inclusive column range. Each row
    /// keeps its row end and prompt mark. An `insert_row_count` of `0` leaves
    /// every cell unchanged.
    pub(crate) fn insert_lines_in_columns(
        &mut self,
        first_row_index: u16,
        last_row_index: u16,
        insert_row_count: u16,
        first_column_index: u16,
        last_column_index: u16,
        fill_style: Style,
    ) {
        let (row_count, column_count) = self.get_grid_dimensions();
        if first_row_index >= row_count
            || last_row_index >= row_count
            || first_row_index > last_row_index
            || column_count == 0
        {
            return;
        }
        let first_column_index = first_column_index.min(column_count - 1) as usize;
        let last_column_index = (last_column_index.min(column_count - 1) as usize) + 1;
        if first_column_index >= last_column_index {
            return;
        }

        let inserted_row_count =
            min(insert_row_count, last_row_index - first_row_index + 1) as usize;
        if inserted_row_count == 0 {
            return;
        }
        let band_rows = &mut self.rows[usize::from(first_row_index)..=usize::from(last_row_index)];
        let band_row_count = band_rows.len();
        for destination_row_index in (inserted_row_count..band_row_count).rev() {
            let (upper_rows, lower_rows) = band_rows.split_at_mut(destination_row_index);
            lower_rows[0]
                .get_cells_mut(first_column_index..last_column_index)
                .swap_with_slice(
                    upper_rows[destination_row_index - inserted_row_count]
                        .get_cells_mut(first_column_index..last_column_index),
                );
        }
        for vacated_row in &mut band_rows[..inserted_row_count] {
            vacated_row
                .get_cells_mut(first_column_index..last_column_index)
                .fill(Cell::build_blank_with_style(fill_style));
        }
    }

    /// Delete `delete_row_count` lines from the band
    /// `[first_row_index, last_row_index]` (both inclusive), shifting
    /// lines below the band upward; blank lines are inserted at the bottom of the
    /// band to preserve the band's height. Cells are filled in `fill_style`
    /// (background-color erase). Coordinates outside the grid are no-ops.
    ///
    /// The removed rows move to the band's bottom and are blanked in place. No
    /// row buffer is allocated or freed.
    pub fn delete_lines(
        &mut self,
        first_row_index: u16,
        last_row_index: u16,
        delete_row_count: u16,
        fill_style: Style,
    ) {
        let (row_count, _) = self.get_grid_dimensions();
        if first_row_index >= row_count
            || last_row_index >= row_count
            || first_row_index > last_row_index
        {
            return;
        }

        // Never remove more lines than the band holds.
        let removed_line_count = min(delete_row_count, last_row_index - first_row_index + 1);
        let band_rows = &mut self.rows[usize::from(first_row_index)..=usize::from(last_row_index)];
        let band_row_count = band_rows.len();
        band_rows.rotate_left(usize::from(removed_line_count));
        for recycled_row in &mut band_rows[band_row_count - usize::from(removed_line_count)..] {
            recycled_row.reset_to_blank(fill_style);
        }
        if removed_line_count > 0 {
            // The row above the band and the row at `last_row_index - removed_line_count` both
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
    ///
    /// The dropped rows move to the band's top and are blanked in place. No row
    /// buffer is allocated or freed.
    pub fn insert_lines(
        &mut self,
        first_row_index: u16,
        last_row_index: u16,
        insert_row_count: u16,
        fill_style: Style,
    ) {
        let (row_count, _) = self.get_grid_dimensions();
        if first_row_index >= row_count
            || last_row_index >= row_count
            || first_row_index > last_row_index
        {
            return;
        }

        // Never insert more lines than the band can hold.
        let inserted_line_count = min(insert_row_count, last_row_index - first_row_index + 1);
        let band_rows = &mut self.rows[usize::from(first_row_index)..=usize::from(last_row_index)];
        band_rows.rotate_right(usize::from(inserted_line_count));
        for recycled_row in &mut band_rows[..usize::from(inserted_line_count)] {
            recycled_row.reset_to_blank(fill_style);
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

/// The stored form of a [`Grid`]: every row as its snapshot, top to bottom.
#[derive(Serialize)]
struct GridFieldsReference {
    rows: Vec<Arc<RowSnapshot>>,
}

impl Serialize for Grid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        GridFieldsReference {
            rows: self.list_row_snapshots().collect(),
        }
        .serialize(serializer)
    }
}

#[derive(Deserialize)]
struct GridFields {
    rows: Vec<RowSnapshot>,
}

impl<'de> Deserialize<'de> for Grid {
    /// Read the stored rows. Every row must hold the first row's column count.
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        let grid_fields = GridFields::deserialize(deserializer)?;
        let rows: Vec<GridRow> = grid_fields
            .rows
            .into_iter()
            .map(|row_snapshot| {
                let mut grid_row = GridRow::from_cells(
                    row_snapshot.decode_row_cells(),
                    row_snapshot.get_row_metadata(),
                );
                grid_row.published_row_snapshot = OnceLock::from(Arc::new(row_snapshot));
                grid_row
            })
            .collect();
        let column_count = rows.first().map_or(0, |grid_row| grid_row.cells.len());
        if rows
            .iter()
            .any(|grid_row| grid_row.cells.len() != column_count)
        {
            return Err(de::Error::custom("grid rows differ in length"));
        }
        Ok(Grid { rows })
    }
}

/// A grid stored with each row as a list of JSON cells and the row metadata
/// in a list of its own, as resume format 4 writes it.
pub(crate) struct CellRowGrid(pub(crate) Grid);

#[derive(Deserialize)]
struct CellRowGridFields {
    rows: Vec<Vec<Cell>>,
    row_metadata: Vec<RowMetadata>,
}

impl<'de> Deserialize<'de> for CellRowGrid {
    /// Read the stored rows. The metadata list holds one entry per row, the
    /// first row holds at most 65 535 cells, and every row holds the first
    /// row's column count.
    fn deserialize<Deserializer>(deserializer: Deserializer) -> Result<Self, Deserializer::Error>
    where
        Deserializer: DeserializerTrait<'de>,
    {
        let grid_fields = CellRowGridFields::deserialize(deserializer)?;
        if grid_fields.row_metadata.len() != grid_fields.rows.len() {
            return Err(de::Error::custom("grid row metadata does not match rows"));
        }
        let column_count = grid_fields.rows.first().map_or(0, Vec::len);
        if column_count > usize::from(u16::MAX) {
            return Err(de::Error::custom("a grid row holds more than 65 535 cells"));
        }
        if grid_fields
            .rows
            .iter()
            .any(|row_cells| row_cells.len() != column_count)
        {
            return Err(de::Error::custom("grid rows differ in length"));
        }
        Ok(CellRowGrid(Grid {
            rows: grid_fields
                .rows
                .into_iter()
                .zip(grid_fields.row_metadata)
                .map(|(row_cells, row_metadata)| GridRow::from_cells(row_cells, row_metadata))
                .collect(),
        }))
    }
}

#[cfg(test)]
pub(crate) mod tests;
