//! Bounded RGBA-to-Sixel encoding.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::Arc;

use koshi_image::{
    checked_rgba_len, validate_dimensions, DecodedImage, GraphicsProtocol,
    MAX_GRAPHICS_TRANSFER_BYTES,
};
use thiserror::Error;

#[cfg(test)]
mod tests;

/// The smallest configurable palette limit.
pub const MIN_PALETTE_COLORS: usize = 2;

/// The largest Sixel palette supported by the encoder.
pub const MAX_PALETTE_COLORS: usize = 256;

/// The default maximum number of colors in an encoded palette.
pub const DEFAULT_PALETTE_COLORS: usize = MAX_PALETTE_COLORS;

/// The largest chunk returned by the encoder.
pub const MAX_SIXEL_CHUNK_BYTES: usize = 16 * 1024;

/// The largest cumulative Sixel transfer emitted by the encoder.
pub const MAX_SIXEL_OUTPUT_BYTES: usize = MAX_GRAPHICS_TRANSFER_BYTES;

/// The largest chunk requested while encoding one Sixel tile.
pub const MAX_SIXEL_TILE_BYTES: usize = MAX_SIXEL_CHUNK_BYTES;

const HISTOGRAM_BUCKETS_PER_CHANNEL: usize = 32;
const HISTOGRAM_BUCKET_COUNT: usize =
    HISTOGRAM_BUCKETS_PER_CHANNEL * HISTOGRAM_BUCKETS_PER_CHANNEL * HISTOGRAM_BUCKETS_PER_CHANNEL;

/// Options that control bounded Sixel encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SixelEncodeOptions {
    /// The largest palette the encoder may emit.
    pub max_colors: usize,
}

impl Default for SixelEncodeOptions {
    fn default() -> Self {
        Self::new(DEFAULT_PALETTE_COLORS)
    }
}

impl SixelEncodeOptions {
    /// Create options with `max_colors` as the palette limit.
    ///
    /// [`PreparedSixelPalette::prepare`] and [`SixelEncoder::with_options`]
    /// reject values outside [`MIN_PALETTE_COLORS`] through
    /// [`MAX_PALETTE_COLORS`].
    #[must_use]
    pub const fn new(max_colors: usize) -> Self {
        SixelEncodeOptions { max_colors }
    }
}

/// A validated palette that can be reused for several image tiles.
#[derive(Debug, Clone)]
pub struct PreparedSixelPalette {
    palette: Palette,
}

impl PreparedSixelPalette {
    /// Validate the image and options, then prepare a bounded palette from
    /// RGBA pixels.
    ///
    /// Returns an error for invalid dimensions, an RGBA length mismatch, an
    /// unsupported palette size, or a failed bounded allocation.
    pub fn prepare(
        image: &DecodedImage,
        background: [u8; 3],
        options: SixelEncodeOptions,
    ) -> Result<Self, SixelEncodeError> {
        validate_options(options)?;
        validate_image(image)?;
        Ok(Self {
            palette: prepare_palette(&image.rgba, background, options.max_colors)?,
        })
    }
}

/// An error raised before or during Sixel encoding.
#[derive(Debug, Error)]
pub enum SixelEncodeError {
    /// The image has zero dimensions, exceeds a side or pixel limit, or has a
    /// dimension multiplication that cannot be represented.
    #[error("Sixel image dimensions are invalid: {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
    /// The RGBA buffer length does not equal `width * height * 4`.
    #[error("Sixel RGBA length is {actual}, expected {expected}")]
    RgbaLengthMismatch { expected: usize, actual: usize },
    /// The requested palette limit is outside the supported range.
    #[error(
        "Sixel palette size {requested} is outside the supported range {MIN_PALETTE_COLORS}..={MAX_PALETTE_COLORS}"
    )]
    InvalidPaletteSize { requested: usize },
    /// The cumulative encoded transfer would exceed the output limit.
    #[error("Sixel output exceeds the {limit}-byte limit")]
    OutputTooLarge { limit: usize },
    /// A fallible allocation for bounded encoder storage failed.
    #[error("Sixel encoder storage could not be allocated")]
    AllocationFailed,
    /// A caller supplied a zero-sized output chunk.
    #[error("Sixel output chunk size must be greater than zero")]
    ZeroChunkSize,
    /// The output writer rejected a byte range.
    #[error("writing Sixel output failed: {0}")]
    Io(#[source] io::Error),
    /// An internal palette lookup could not find a visible pixel color.
    #[error("Sixel pixel color could not be mapped to the prepared palette")]
    PaletteMapping,
    /// Output generation has failed and this encoder cannot continue.
    #[error("Sixel encoder has failed and cannot continue")]
    EncoderFailed,
}

impl From<io::Error> for SixelEncodeError {
    fn from(error: io::Error) -> Self {
        SixelEncodeError::Io(error)
    }
}

/// A prepared Sixel encoder with bounded incremental output.
///
/// Construction validates and scans the shared image, prepares its bounded
/// palette, and performs no I/O or threading. The image is retained through
/// its `Arc`; generated output stays in a queue no larger than
/// [`MAX_SIXEL_CHUNK_BYTES`].
#[derive(Debug)]
pub struct SixelEncoder {
    image: Arc<DecodedImage>,
    background: [u8; 3],
    width: usize,
    height: usize,
    palette: Palette,
    header: Vec<u8>,
    header_offset: usize,
    next_band_start: usize,
    band: Option<BandState>,
    saw_sixel: bool,
    terminator_offset: usize,
    pending: Vec<u8>,
    pending_offset: usize,
    generated_bytes: usize,
    generation_error: Option<SixelEncodeError>,
    generation_failed: bool,
    finished: bool,
}

impl SixelEncoder {
    /// Prepare a Sixel encoder with the default 256-color palette limit.
    pub fn new(image: Arc<DecodedImage>, background: [u8; 3]) -> Result<Self, SixelEncodeError> {
        Self::with_options(image, background, SixelEncodeOptions::default())
    }

    /// Prepare a Sixel encoder with an explicit palette limit.
    pub fn with_options(
        image: Arc<DecodedImage>,
        background: [u8; 3],
        options: SixelEncodeOptions,
    ) -> Result<Self, SixelEncodeError> {
        let palette = PreparedSixelPalette::prepare(&image, background, options)?;
        Self::with_palette(image, background, palette)
    }

    /// Prepare a Sixel encoder with a palette shared by related image tiles.
    pub fn with_palette(
        image: Arc<DecodedImage>,
        background: [u8; 3],
        palette: PreparedSixelPalette,
    ) -> Result<Self, SixelEncodeError> {
        let (width, height) = validate_image(&image)?;
        let header = build_header(width, height, &palette.palette)?;
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(MAX_SIXEL_CHUNK_BYTES)
            .map_err(|_| SixelEncodeError::AllocationFailed)?;
        Ok(SixelEncoder {
            image,
            background,
            width,
            height,
            palette: palette.palette,
            header,
            header_offset: 0,
            next_band_start: 0,
            band: None,
            saw_sixel: false,
            terminator_offset: 0,
            pending,
            pending_offset: 0,
            generated_bytes: 0,
            generation_error: None,
            generation_failed: false,
            finished: false,
        })
    }

    /// Return and consume the next output chunk.
    ///
    /// A `max_bytes` value of zero returns [`SixelEncodeError::ZeroChunkSize`].
    /// Larger values are clamped to [`MAX_SIXEL_CHUNK_BYTES`]. A successful
    /// call advances the encoder past the returned bytes; `None` marks the end.
    pub fn next_chunk(&mut self, max_bytes: usize) -> Result<Option<&[u8]>, SixelEncodeError> {
        let max_bytes = chunk_limit(max_bytes)?;
        self.prepare_pending()?;
        if self.pending_offset == self.pending.len() {
            return Ok(None);
        }
        let end = self.pending_offset + max_bytes.min(self.pending.len() - self.pending_offset);
        let start = self.pending_offset;
        self.pending_offset = end;
        Ok(Some(&self.pending[start..end]))
    }

    /// Write all output in chunks of [`MAX_SIXEL_CHUNK_BYTES`].
    ///
    /// Returns [`SixelEncodeError::Io`] when the writer rejects a chunk.
    pub fn write_to<W: Write>(&mut self, writer: &mut W) -> Result<(), SixelEncodeError> {
        while self.write_next_chunk(writer, MAX_SIXEL_CHUNK_BYTES)? {}
        Ok(())
    }

    /// Write one output chunk and advance only after `write_all` succeeds.
    ///
    /// A `max_bytes` value of zero returns [`SixelEncodeError::ZeroChunkSize`].
    /// A writer can report an error after writing part of the slice; the
    /// pending bytes stay unchanged. Abort and discard the open transfer, then
    /// restart with a new encoder instead of resuming this encoder, which would
    /// emit an incomplete Sixel string.
    pub fn write_next_chunk<W: Write>(
        &mut self,
        writer: &mut W,
        max_bytes: usize,
    ) -> Result<bool, SixelEncodeError> {
        let max_bytes = chunk_limit(max_bytes)?;
        self.prepare_pending()?;
        if self.pending_offset == self.pending.len() {
            return Ok(false);
        }
        let end = self.pending_offset + max_bytes.min(self.pending.len() - self.pending_offset);
        writer.write_all(&self.pending[self.pending_offset..end])?;
        self.pending_offset = end;
        Ok(true)
    }

    fn prepare_pending(&mut self) -> Result<(), SixelEncodeError> {
        if self.pending_offset == self.pending.len() {
            self.pending.clear();
            self.pending_offset = 0;
        }
        if !self.pending.is_empty() {
            return Ok(());
        }
        if self.generation_failed {
            if let Some(error) = self.generation_error.take() {
                return Err(error);
            }
            return Err(SixelEncodeError::EncoderFailed);
        }
        if self.finished {
            return Ok(());
        }
        while self.pending.len() < MAX_SIXEL_CHUNK_BYTES {
            match self.produce_byte() {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    self.generation_failed = true;
                    self.generation_error = Some(error);
                    if self.pending.is_empty() {
                        return Err(self
                            .generation_error
                            .take()
                            .expect("generation error is stored before it is returned"));
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    fn produce_byte(&mut self) -> Result<bool, SixelEncodeError> {
        loop {
            if self.header_offset < self.header.len() {
                let byte = self.header[self.header_offset];
                self.emit_byte(byte)?;
                self.header_offset += 1;
                return Ok(true);
            }

            if let Some(band) = self.band.as_mut() {
                if let Some(byte) = band.next_byte() {
                    self.emit_byte(byte)?;
                    return Ok(true);
                }
                self.band = None;
                if self.next_band_start < self.height {
                    self.emit_byte(b'-')?;
                    return Ok(true);
                }
                continue;
            }

            if self.next_band_start < self.height {
                let band_start = self.next_band_start;
                let band_height = (self.height - band_start).min(6);
                self.next_band_start += band_height;
                let band = build_band(
                    &self.image,
                    self.width,
                    band_start,
                    band_height,
                    self.background,
                    &self.palette,
                )?;
                if band.has_data() {
                    self.saw_sixel = true;
                } else if !self.saw_sixel {
                    self.saw_sixel = true;
                    self.band = Some(band);
                    self.emit_byte(b'?')?;
                    return Ok(true);
                }
                self.band = Some(band);
                continue;
            }

            if self.terminator_offset < 2 {
                let byte = [0x1b, b'\\'][self.terminator_offset];
                self.emit_byte(byte)?;
                self.terminator_offset += 1;
                if self.terminator_offset == 2 {
                    self.finished = true;
                }
                return Ok(true);
            }
            self.finished = true;
            return Ok(false);
        }
    }

    fn emit_byte(&mut self, byte: u8) -> Result<(), SixelEncodeError> {
        if self.generated_bytes >= MAX_SIXEL_OUTPUT_BYTES {
            return Err(SixelEncodeError::OutputTooLarge {
                limit: MAX_SIXEL_OUTPUT_BYTES,
            });
        }
        if self.pending.len() >= MAX_SIXEL_CHUNK_BYTES {
            return Err(SixelEncodeError::OutputTooLarge {
                limit: MAX_SIXEL_CHUNK_BYTES,
            });
        }
        self.pending.push(byte);
        self.generated_bytes += 1;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn inject_generation_failure_for_test(&mut self) {
        self.generation_failed = true;
        self.generation_error = Some(SixelEncodeError::PaletteMapping);
    }
}

#[derive(Debug, Clone)]
struct Palette {
    colors: Vec<[u8; 3]>,
    mapping: PaletteMapping,
}

#[derive(Debug, Clone)]
enum PaletteMapping {
    Exact(BTreeMap<[u8; 3], u16>),
    Quantized(Vec<u16>),
}

impl Palette {
    fn index(&self, color: [u8; 3]) -> Result<usize, SixelEncodeError> {
        match &self.mapping {
            PaletteMapping::Exact(mapping) => mapping
                .get(&color)
                .copied()
                .map(usize::from)
                .ok_or(SixelEncodeError::PaletteMapping),
            PaletteMapping::Quantized(mapping) => mapping
                .get(histogram_index(color))
                .copied()
                .filter(|index| usize::from(*index) < self.colors.len())
                .map(usize::from)
                .ok_or(SixelEncodeError::PaletteMapping),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct HistogramBin {
    count: u64,
    sums: [u64; 3],
}

impl HistogramBin {
    fn add(&mut self, color: [u8; 3]) {
        self.count = self.count.saturating_add(1);
        for (sum, channel) in self.sums.iter_mut().zip(color) {
            *sum = sum.saturating_add(u64::from(channel));
        }
    }

    fn average(self) -> [u8; 3] {
        let count = self.count.max(1);
        [
            rounded_average(self.sums[0], count),
            rounded_average(self.sums[1], count),
            rounded_average(self.sums[2], count),
        ]
    }
}

#[derive(Debug, Clone, Copy)]
struct HistogramColor {
    bin: usize,
    color: [u8; 3],
    weight: u64,
}

#[derive(Debug)]
struct ColorBox {
    members: Vec<usize>,
    minimum: [u8; 3],
    maximum: [u8; 3],
    weight: u64,
}

#[derive(Debug, Clone, Copy)]
struct BandEntry {
    x: usize,
    bits: u8,
}

#[derive(Debug)]
struct BandState {
    entries: Vec<Vec<BandEntry>>,
    color_index: usize,
    entry_index: usize,
    position: usize,
    stage: BandStage,
    emitted_color: bool,
    token: [u8; 32],
    token_len: usize,
    token_offset: usize,
    has_data: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BandStage {
    ColorSeparator,
    ColorNumber,
    Pixels,
}

impl BandState {
    fn new(entries: Vec<Vec<BandEntry>>) -> Self {
        let has_data = entries.iter().any(|entries| !entries.is_empty());
        BandState {
            entries,
            color_index: 0,
            entry_index: 0,
            position: 0,
            stage: BandStage::ColorSeparator,
            emitted_color: false,
            token: [0; 32],
            token_len: 0,
            token_offset: 0,
            has_data,
        }
    }

    fn has_data(&self) -> bool {
        self.has_data
    }

    fn next_byte(&mut self) -> Option<u8> {
        loop {
            if self.token_offset < self.token_len {
                let byte = self.token[self.token_offset];
                self.token_offset += 1;
                return Some(byte);
            }

            while self.color_index < self.entries.len() && self.entries[self.color_index].is_empty()
            {
                self.color_index += 1;
                self.entry_index = 0;
                self.position = 0;
                self.stage = BandStage::ColorSeparator;
            }
            if self.color_index == self.entries.len() {
                return None;
            }

            match self.stage {
                BandStage::ColorSeparator => {
                    self.stage = BandStage::ColorNumber;
                    if self.emitted_color {
                        self.set_token_byte(b'$');
                    }
                    continue;
                }
                BandStage::ColorNumber => {
                    self.stage = BandStage::Pixels;
                    self.emitted_color = true;
                    self.set_color_token(self.color_index);
                    continue;
                }
                BandStage::Pixels => {}
            }

            let (value, run, next_entry, next_position) = {
                let entries = &self.entries[self.color_index];
                if self.entry_index >= entries.len() {
                    self.color_index += 1;
                    self.entry_index = 0;
                    self.position = 0;
                    self.stage = BandStage::ColorSeparator;
                    continue;
                }
                let entry = entries[self.entry_index];
                if self.position < entry.x {
                    (0, entry.x - self.position, self.entry_index, entry.x)
                } else {
                    let mut last_x = entry.x;
                    let mut next_entry = self.entry_index + 1;
                    while next_entry < entries.len()
                        && entries[next_entry].x == last_x + 1
                        && entries[next_entry].bits == entry.bits
                    {
                        last_x += 1;
                        next_entry += 1;
                    }
                    (entry.bits, last_x - entry.x + 1, next_entry, last_x + 1)
                }
            };
            self.entry_index = next_entry;
            self.position = next_position;
            self.set_pixel_token(run, value);
        }
    }

    fn set_token_byte(&mut self, byte: u8) {
        self.token[0] = byte;
        self.token_len = 1;
        self.token_offset = 0;
    }

    fn set_color_token(&mut self, color_index: usize) {
        self.token[0] = b'#';
        let digits = write_usize(&mut self.token[1..], color_index);
        self.token_len = digits + 1;
        self.token_offset = 0;
    }

    fn set_pixel_token(&mut self, run: usize, value: u8) {
        self.token_offset = 0;
        if run > 1 {
            self.token[0] = b'!';
            let digits = write_usize(&mut self.token[1..], run);
            self.token[digits + 1] = b'?'.saturating_add(value);
            self.token_len = digits + 2;
        } else {
            self.token[0] = b'?'.saturating_add(value);
            self.token_len = 1;
        }
    }
}

fn validate_options(options: SixelEncodeOptions) -> Result<(), SixelEncodeError> {
    if !(MIN_PALETTE_COLORS..=MAX_PALETTE_COLORS).contains(&options.max_colors) {
        return Err(SixelEncodeError::InvalidPaletteSize {
            requested: options.max_colors,
        });
    }
    Ok(())
}

fn validate_image(image: &DecodedImage) -> Result<(usize, usize), SixelEncodeError> {
    let width = usize::try_from(image.width).map_err(|_| SixelEncodeError::InvalidDimensions {
        width: image.width,
        height: image.height,
    })?;
    let height =
        usize::try_from(image.height).map_err(|_| SixelEncodeError::InvalidDimensions {
            width: image.width,
            height: image.height,
        })?;
    validate_dimensions(GraphicsProtocol::Sixel, width, height).map_err(|_| {
        SixelEncodeError::InvalidDimensions {
            width: image.width,
            height: image.height,
        }
    })?;
    let expected = checked_rgba_len(GraphicsProtocol::Sixel, width, height).map_err(|_| {
        SixelEncodeError::InvalidDimensions {
            width: image.width,
            height: image.height,
        }
    })?;
    if image.rgba.len() != expected {
        return Err(SixelEncodeError::RgbaLengthMismatch {
            expected,
            actual: image.rgba.len(),
        });
    }
    Ok((width, height))
}

fn chunk_limit(max_bytes: usize) -> Result<usize, SixelEncodeError> {
    if max_bytes == 0 {
        return Err(SixelEncodeError::ZeroChunkSize);
    }
    Ok(max_bytes.min(MAX_SIXEL_CHUNK_BYTES))
}

fn build_header(
    width: usize,
    height: usize,
    palette: &Palette,
) -> Result<Vec<u8>, SixelEncodeError> {
    let estimate = 64usize.saturating_add(palette.colors.len().saturating_mul(20));
    let mut header = Vec::new();
    header
        .try_reserve(estimate.min(MAX_SIXEL_CHUNK_BYTES))
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    append_header_bytes(&mut header, b"\x1bP7;1q")?;
    append_header_byte(&mut header, b'"')?;
    append_header_bytes(&mut header, b"1;1;")?;
    append_header_usize(&mut header, width)?;
    append_header_byte(&mut header, b';')?;
    append_header_usize(&mut header, height)?;
    for (index, color) in palette.colors.iter().enumerate() {
        append_header_byte(&mut header, b'#')?;
        append_header_usize(&mut header, index)?;
        append_header_bytes(&mut header, b";2;")?;
        append_header_usize(&mut header, usize::from(color[0]))?;
        append_header_byte(&mut header, b';')?;
        append_header_usize(&mut header, usize::from(color[1]))?;
        append_header_byte(&mut header, b';')?;
        append_header_usize(&mut header, usize::from(color[2]))?;
    }
    Ok(header)
}

fn append_header_byte(output: &mut Vec<u8>, byte: u8) -> Result<(), SixelEncodeError> {
    append_header_bytes(output, &[byte])
}

fn append_header_usize(output: &mut Vec<u8>, value: usize) -> Result<(), SixelEncodeError> {
    let mut digits = [0u8; 20];
    let count = write_usize(&mut digits, value);
    append_header_bytes(output, &digits[..count])
}

fn append_header_bytes(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SixelEncodeError> {
    let new_len =
        output
            .len()
            .checked_add(bytes.len())
            .ok_or(SixelEncodeError::OutputTooLarge {
                limit: MAX_SIXEL_OUTPUT_BYTES,
            })?;
    if new_len > MAX_SIXEL_CHUNK_BYTES {
        return Err(SixelEncodeError::OutputTooLarge {
            limit: MAX_SIXEL_OUTPUT_BYTES,
        });
    }
    if output.capacity().saturating_sub(output.len()) < bytes.len() {
        output
            .try_reserve_exact(bytes.len())
            .map_err(|_| SixelEncodeError::AllocationFailed)?;
    }
    output.extend_from_slice(bytes);
    Ok(())
}

fn write_usize(output: &mut [u8], value: usize) -> usize {
    let mut digits = [0u8; 20];
    let mut position = digits.len();
    let mut value = value;
    if value == 0 {
        position -= 1;
        digits[position] = b'0';
    } else {
        while value > 0 {
            position -= 1;
            digits[position] = b'0' + (value % 10) as u8;
            value /= 10;
        }
    }
    let count = digits.len() - position;
    output[..count].copy_from_slice(&digits[position..]);
    count
}

fn prepare_palette(
    rgba: &[u8],
    background: [u8; 3],
    max_colors: usize,
) -> Result<Palette, SixelEncodeError> {
    let mut seen = BTreeSet::new();
    let mut colors = Vec::new();
    colors
        .try_reserve_exact(max_colors)
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    for pixel in rgba.chunks_exact(4) {
        let Some(color) = blended_percentage(pixel, background) else {
            continue;
        };
        if seen.insert(color) {
            colors.push(color);
            if colors.len() > max_colors {
                return prepare_quantized_palette(rgba, background, max_colors);
            }
        }
    }
    let mut mapping = BTreeMap::new();
    for (index, color) in colors.iter().copied().enumerate() {
        mapping.insert(
            color,
            u16::try_from(index).map_err(|_| SixelEncodeError::PaletteMapping)?,
        );
    }
    Ok(Palette {
        colors,
        mapping: PaletteMapping::Exact(mapping),
    })
}

fn prepare_quantized_palette(
    rgba: &[u8],
    background: [u8; 3],
    max_colors: usize,
) -> Result<Palette, SixelEncodeError> {
    let mut bins = Vec::new();
    bins.try_reserve_exact(HISTOGRAM_BUCKET_COUNT)
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    bins.resize(HISTOGRAM_BUCKET_COUNT, HistogramBin::default());
    for pixel in rgba.chunks_exact(4) {
        let Some(color) = blended_percentage(pixel, background) else {
            continue;
        };
        bins[histogram_index(color)].add(color);
    }

    let mut samples = Vec::new();
    samples
        .try_reserve_exact(HISTOGRAM_BUCKET_COUNT)
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    for (bin, histogram) in bins.into_iter().enumerate() {
        if histogram.count == 0 {
            continue;
        }
        samples.push(HistogramColor {
            bin,
            color: histogram.average(),
            weight: histogram.count,
        });
    }
    if samples.is_empty() {
        return Ok(Palette {
            colors: Vec::new(),
            mapping: PaletteMapping::Exact(BTreeMap::new()),
        });
    }

    let mut initial_members = Vec::new();
    initial_members
        .try_reserve_exact(samples.len())
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    initial_members.extend(0..samples.len());
    let mut boxes = Vec::new();
    boxes
        .try_reserve_exact(max_colors)
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    boxes.push(make_color_box(initial_members, &samples));
    while boxes.len() < max_colors {
        let Some(box_index) = boxes
            .iter()
            .enumerate()
            .filter(|(_, color_box)| color_box.members.len() > 1 && color_box_range(color_box) > 0)
            .max_by_key(|(index, color_box)| {
                (
                    color_box_range(color_box),
                    color_box.weight,
                    std::cmp::Reverse(*index),
                )
            })
            .map(|(index, _)| index)
        else {
            break;
        };
        let channel = split_channel(&boxes[box_index]);
        let color_box = &mut boxes[box_index];
        color_box.members.sort_by_key(|sample_index| {
            let sample = samples[*sample_index];
            (
                sample.color[channel],
                sample.color[(channel + 1) % 3],
                sample.color[(channel + 2) % 3],
                sample.bin,
            )
        });
        let target = color_box.weight.saturating_add(1) / 2;
        let mut cumulative = 0u64;
        let mut split_at = color_box.members.len() / 2;
        for (position, sample_index) in color_box.members.iter().enumerate() {
            cumulative = cumulative.saturating_add(samples[*sample_index].weight);
            if cumulative >= target {
                split_at = position + 1;
                break;
            }
        }
        split_at = split_at.clamp(1, color_box.members.len() - 1);

        let members = std::mem::take(&mut color_box.members);
        let right_len = members.len() - split_at;
        let mut left_members = Vec::new();
        left_members
            .try_reserve_exact(split_at)
            .map_err(|_| SixelEncodeError::AllocationFailed)?;
        let mut right_members = Vec::new();
        right_members
            .try_reserve_exact(right_len)
            .map_err(|_| SixelEncodeError::AllocationFailed)?;
        for (position, member) in members.into_iter().enumerate() {
            if position < split_at {
                left_members.push(member);
            } else {
                right_members.push(member);
            }
        }
        let left_box = make_color_box(left_members, &samples);
        let right_box = make_color_box(right_members, &samples);
        boxes[box_index] = left_box;
        boxes.push(right_box);
    }

    let mut colors = Vec::new();
    colors
        .try_reserve_exact(boxes.len())
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    let mut mapping = Vec::new();
    mapping
        .try_reserve_exact(HISTOGRAM_BUCKET_COUNT)
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    mapping.resize(HISTOGRAM_BUCKET_COUNT, u16::MAX);
    for (index, color_box) in boxes.into_iter().enumerate() {
        let mut sums = [0u64; 3];
        let mut weight = 0u64;
        for sample_index in color_box.members {
            let sample = samples[sample_index];
            weight = weight.saturating_add(sample.weight);
            for (sum, channel) in sums.iter_mut().zip(sample.color) {
                *sum = sum.saturating_add(u64::from(channel).saturating_mul(sample.weight));
            }
            mapping[sample.bin] =
                u16::try_from(index).map_err(|_| SixelEncodeError::PaletteMapping)?;
        }
        let weight = weight.max(1);
        colors.push([
            rounded_average(sums[0], weight),
            rounded_average(sums[1], weight),
            rounded_average(sums[2], weight),
        ]);
    }
    Ok(Palette {
        colors,
        mapping: PaletteMapping::Quantized(mapping),
    })
}

fn make_color_box(members: Vec<usize>, samples: &[HistogramColor]) -> ColorBox {
    let mut minimum = [u8::MAX; 3];
    let mut maximum = [0; 3];
    let mut weight = 0u64;
    for sample_index in &members {
        let sample = samples[*sample_index];
        weight = weight.saturating_add(sample.weight);
        for channel in 0..3 {
            minimum[channel] = minimum[channel].min(sample.color[channel]);
            maximum[channel] = maximum[channel].max(sample.color[channel]);
        }
    }
    ColorBox {
        members,
        minimum,
        maximum,
        weight,
    }
}

fn color_box_range(color_box: &ColorBox) -> u8 {
    color_box
        .maximum
        .iter()
        .zip(color_box.minimum)
        .map(|(maximum, minimum)| maximum.saturating_sub(minimum))
        .max()
        .unwrap_or(0)
}

fn split_channel(color_box: &ColorBox) -> usize {
    let ranges = [
        color_box.maximum[0].saturating_sub(color_box.minimum[0]),
        color_box.maximum[1].saturating_sub(color_box.minimum[1]),
        color_box.maximum[2].saturating_sub(color_box.minimum[2]),
    ];
    ranges
        .iter()
        .enumerate()
        .max_by_key(|(channel, range)| (**range, std::cmp::Reverse(*channel)))
        .map(|(channel, _)| channel)
        .unwrap_or(0)
}

fn build_band(
    image: &DecodedImage,
    width: usize,
    band_start: usize,
    band_height: usize,
    background: [u8; 3],
    palette: &Palette,
) -> Result<BandState, SixelEncodeError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(palette.colors.len())
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    entries.resize_with(palette.colors.len(), Vec::new);

    for row in 0..band_height {
        let y = band_start + row;
        for x in 0..width {
            let offset = (y * width + x) * 4;
            let Some(color) = blended_percentage(&image.rgba[offset..offset + 4], background)
            else {
                continue;
            };
            let index = palette.index(color)?;
            let color_entries = &mut entries[index];
            color_entries
                .try_reserve(1)
                .map_err(|_| SixelEncodeError::AllocationFailed)?;
            color_entries.push(BandEntry { x, bits: 1 << row });
        }
    }

    for color_entries in &mut entries {
        color_entries.sort_unstable_by_key(|entry| entry.x);
        let mut length = 0;
        for read in 0..color_entries.len() {
            let entry = color_entries[read];
            if length > 0 && color_entries[length - 1].x == entry.x {
                color_entries[length - 1].bits |= entry.bits;
            } else {
                color_entries[length] = entry;
                length += 1;
            }
        }
        color_entries.truncate(length);
    }
    Ok(BandState::new(entries))
}

fn blended_percentage(pixel: &[u8], background: [u8; 3]) -> Option<[u8; 3]> {
    let alpha = pixel[3];
    if alpha == 0 {
        return None;
    }
    let inverse = 255u16 - u16::from(alpha);
    Some([
        byte_to_percentage(blend_channel(pixel[0], background[0], alpha, inverse)),
        byte_to_percentage(blend_channel(pixel[1], background[1], alpha, inverse)),
        byte_to_percentage(blend_channel(pixel[2], background[2], alpha, inverse)),
    ])
}

fn blend_channel(source: u8, background: u8, alpha: u8, inverse: u16) -> u8 {
    let numerator =
        u32::from(source) * u32::from(alpha) + u32::from(background) * u32::from(inverse) + 127;
    (numerator / 255) as u8
}

fn byte_to_percentage(value: u8) -> u8 {
    ((u16::from(value) * 100 + 127) / 255) as u8
}

fn rounded_average(sum: u64, count: u64) -> u8 {
    ((sum.saturating_add(count / 2)) / count.max(1)).min(100) as u8
}

fn histogram_index(color: [u8; 3]) -> usize {
    let red = usize::from(color[0]) * HISTOGRAM_BUCKETS_PER_CHANNEL / 101;
    let green = usize::from(color[1]) * HISTOGRAM_BUCKETS_PER_CHANNEL / 101;
    let blue = usize::from(color[2]) * HISTOGRAM_BUCKETS_PER_CHANNEL / 101;
    (red * HISTOGRAM_BUCKETS_PER_CHANNEL + green) * HISTOGRAM_BUCKETS_PER_CHANNEL + blue
}
