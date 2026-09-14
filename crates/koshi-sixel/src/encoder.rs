//! Bounded RGBA-to-Sixel encoding.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::Arc;

use koshi_image::{
    compute_rgba_byte_count, validate_image_dimensions, DecodedImage, GraphicsProtocol,
    MAX_GRAPHICS_TRANSFER_BYTE_COUNT,
};
use thiserror::Error;

#[cfg(test)]
mod tests;

/// The smallest configurable palette limit.
pub const MIN_PALETTE_COLOR_COUNT: usize = 2;

/// The largest Sixel palette supported by the encoder.
pub const MAX_PALETTE_COLOR_COUNT: usize = 256;

/// The default maximum number of colors in an encoded palette.
pub const DEFAULT_PALETTE_COLOR_COUNT: usize = MAX_PALETTE_COLOR_COUNT;

/// The largest chunk returned by the encoder.
pub const MAX_SIXEL_CHUNK_BYTE_COUNT: usize = 16 * 1024;

/// The largest cumulative Sixel transfer emitted by the encoder.
pub const MAX_SIXEL_OUTPUT_BYTE_COUNT: usize = MAX_GRAPHICS_TRANSFER_BYTE_COUNT;

/// The largest chunk requested while encoding one Sixel tile.
pub const MAX_SIXEL_TILE_BYTE_COUNT: usize = MAX_SIXEL_CHUNK_BYTE_COUNT;

const HISTOGRAM_BUCKET_COUNT_PER_CHANNEL: usize = 32;
const HISTOGRAM_BUCKET_COUNT: usize = HISTOGRAM_BUCKET_COUNT_PER_CHANNEL
    * HISTOGRAM_BUCKET_COUNT_PER_CHANNEL
    * HISTOGRAM_BUCKET_COUNT_PER_CHANNEL;

/// Options that control bounded Sixel encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SixelEncodeOptions {
    /// The largest palette the encoder may emit.
    pub maximum_palette_color_count: usize,
}

impl Default for SixelEncodeOptions {
    fn default() -> Self {
        Self::with_maximum_palette_color_count(DEFAULT_PALETTE_COLOR_COUNT)
    }
}

impl SixelEncodeOptions {
    /// Create options with `maximum_palette_color_count` as the palette limit.
    ///
    /// [`PreparedSixelPalette::prepare`] and [`SixelEncoder::with_options`]
    /// reject values outside [`MIN_PALETTE_COLOR_COUNT`] through
    /// [`MAX_PALETTE_COLOR_COUNT`].
    #[must_use]
    pub const fn with_maximum_palette_color_count(maximum_palette_color_count: usize) -> Self {
        SixelEncodeOptions {
            maximum_palette_color_count,
        }
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
        decoded_image: &DecodedImage,
        background: [u8; 3],
        encode_options: SixelEncodeOptions,
    ) -> Result<Self, SixelEncodeError> {
        validate_sixel_encode_options(encode_options)?;
        validate_decoded_image(decoded_image)?;
        Ok(Self {
            palette: prepare_sixel_palette(
                &decoded_image.rgba_bytes,
                background,
                encode_options.maximum_palette_color_count,
            )?,
        })
    }
}

/// An error raised before or during Sixel encoding.
#[derive(Debug, Error)]
pub enum SixelEncodeError {
    /// The image has zero dimensions, exceeds a side or pixel limit, or has a
    /// dimension multiplication that cannot be represented.
    #[error("Sixel image dimensions are invalid: {pixel_width}x{pixel_height}")]
    InvalidDimensions { pixel_width: u32, pixel_height: u32 },
    /// The RGBA buffer length does not equal `pixel_width * pixel_height * 4`.
    #[error("Sixel RGBA length is {actual_rgba_byte_count}, expected {expected_rgba_byte_count}")]
    RgbaLengthMismatch {
        expected_rgba_byte_count: usize,
        actual_rgba_byte_count: usize,
    },
    /// The requested palette limit is outside the supported range.
    #[error(
        "Sixel palette size {requested_palette_color_count} is outside the supported range {MIN_PALETTE_COLOR_COUNT}..={MAX_PALETTE_COLOR_COUNT}"
    )]
    InvalidPaletteSize {
        requested_palette_color_count: usize,
    },
    /// The cumulative encoded transfer would exceed the output limit.
    #[error("Sixel output exceeds the {maximum_byte_count}-byte limit")]
    OutputTooLarge { maximum_byte_count: usize },
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
    fn from(io_error: io::Error) -> Self {
        SixelEncodeError::Io(io_error)
    }
}

/// A prepared Sixel encoder with bounded incremental output.
///
/// Construction validates and scans the shared image, prepares its bounded
/// palette, and performs no I/O or threading. The image is retained through
/// its `Arc`; generated output stays in a queue no larger than
/// [`MAX_SIXEL_CHUNK_BYTE_COUNT`].
#[derive(Debug)]
pub struct SixelEncoder {
    decoded_image: Arc<DecodedImage>,
    background: [u8; 3],
    image_width_pixels: usize,
    image_height_pixels: usize,
    palette: Palette,
    header_bytes: Vec<u8>,
    header_byte_offset: usize,
    next_band_start_row: usize,
    current_band: Option<BandState>,
    has_emitted_sixel: bool,
    terminator_byte_offset: usize,
    pending_output_bytes: Vec<u8>,
    pending_output_byte_offset: usize,
    generated_byte_count: usize,
    generation_error: Option<SixelEncodeError>,
    is_generation_failed: bool,
    is_finished: bool,
}

impl SixelEncoder {
    /// Prepare a Sixel encoder with the default 256-color palette limit.
    pub fn from_image(
        decoded_image: Arc<DecodedImage>,
        background: [u8; 3],
    ) -> Result<Self, SixelEncodeError> {
        Self::with_options(decoded_image, background, SixelEncodeOptions::default())
    }

    /// Prepare a Sixel encoder with an explicit palette limit.
    pub fn with_options(
        decoded_image: Arc<DecodedImage>,
        background: [u8; 3],
        encode_options: SixelEncodeOptions,
    ) -> Result<Self, SixelEncodeError> {
        let palette = PreparedSixelPalette::prepare(&decoded_image, background, encode_options)?;
        Self::with_palette(decoded_image, background, palette)
    }

    /// Prepare a Sixel encoder with a palette shared by related image tiles.
    pub fn with_palette(
        decoded_image: Arc<DecodedImage>,
        background: [u8; 3],
        palette: PreparedSixelPalette,
    ) -> Result<Self, SixelEncodeError> {
        let (image_width_pixels, image_height_pixels) = validate_decoded_image(&decoded_image)?;
        let header_bytes =
            build_sixel_header(image_width_pixels, image_height_pixels, &palette.palette)?;
        let mut pending_output_bytes = Vec::new();
        pending_output_bytes
            .try_reserve_exact(MAX_SIXEL_CHUNK_BYTE_COUNT)
            .map_err(|_| SixelEncodeError::AllocationFailed)?;
        Ok(SixelEncoder {
            decoded_image,
            background,
            image_width_pixels,
            image_height_pixels,
            palette: palette.palette,
            header_bytes,
            header_byte_offset: 0,
            next_band_start_row: 0,
            current_band: None,
            has_emitted_sixel: false,
            terminator_byte_offset: 0,
            pending_output_bytes,
            pending_output_byte_offset: 0,
            generated_byte_count: 0,
            generation_error: None,
            is_generation_failed: false,
            is_finished: false,
        })
    }

    /// Return and consume the next output chunk.
    ///
    /// A `maximum_byte_count` value of zero returns [`SixelEncodeError::ZeroChunkSize`].
    /// Larger values are clamped to [`MAX_SIXEL_CHUNK_BYTE_COUNT`]. A successful
    /// call advances the encoder past the returned bytes; `None` marks the end.
    pub fn take_next_chunk(
        &mut self,
        maximum_byte_count: usize,
    ) -> Result<Option<&[u8]>, SixelEncodeError> {
        let maximum_byte_count = resolve_chunk_byte_count(maximum_byte_count)?;
        self.prepare_pending_output()?;
        if self.pending_output_byte_offset == self.pending_output_bytes.len() {
            return Ok(None);
        }
        let output_end_index = self.pending_output_byte_offset
            + maximum_byte_count
                .min(self.pending_output_bytes.len() - self.pending_output_byte_offset);
        let output_start_index = self.pending_output_byte_offset;
        self.pending_output_byte_offset = output_end_index;
        Ok(Some(
            &self.pending_output_bytes[output_start_index..output_end_index],
        ))
    }

    /// Write all output in chunks of [`MAX_SIXEL_CHUNK_BYTE_COUNT`].
    ///
    /// Returns [`SixelEncodeError::Io`] when the writer rejects a chunk.
    pub fn write_to<Writer: Write>(&mut self, writer: &mut Writer) -> Result<(), SixelEncodeError> {
        while self.write_next_chunk(writer, MAX_SIXEL_CHUNK_BYTE_COUNT)? {}
        Ok(())
    }

    /// Write one output chunk and advance only after `write_all` succeeds.
    ///
    /// A `maximum_byte_count` value of zero returns [`SixelEncodeError::ZeroChunkSize`].
    /// A writer can report an error after writing part of the slice; the
    /// pending bytes stay unchanged. Abort and discard the open transfer, then
    /// restart with a new encoder instead of resuming this encoder, which would
    /// emit an incomplete Sixel string.
    pub fn write_next_chunk<Writer: Write>(
        &mut self,
        writer: &mut Writer,
        maximum_byte_count: usize,
    ) -> Result<bool, SixelEncodeError> {
        let maximum_byte_count = resolve_chunk_byte_count(maximum_byte_count)?;
        self.prepare_pending_output()?;
        if self.pending_output_byte_offset == self.pending_output_bytes.len() {
            return Ok(false);
        }
        let output_end_index = self.pending_output_byte_offset
            + maximum_byte_count
                .min(self.pending_output_bytes.len() - self.pending_output_byte_offset);
        writer.write_all(
            &self.pending_output_bytes[self.pending_output_byte_offset..output_end_index],
        )?;
        self.pending_output_byte_offset = output_end_index;
        Ok(true)
    }

    fn prepare_pending_output(&mut self) -> Result<(), SixelEncodeError> {
        if self.pending_output_byte_offset == self.pending_output_bytes.len() {
            self.pending_output_bytes.clear();
            self.pending_output_byte_offset = 0;
        }
        if !self.pending_output_bytes.is_empty() {
            return Ok(());
        }
        if self.is_generation_failed {
            if let Some(generation_error) = self.generation_error.take() {
                return Err(generation_error);
            }
            return Err(SixelEncodeError::EncoderFailed);
        }
        if self.is_finished {
            return Ok(());
        }
        while self.pending_output_bytes.len() < MAX_SIXEL_CHUNK_BYTE_COUNT {
            match self.produce_next_byte() {
                Ok(true) => {}
                Ok(false) => break,
                Err(generation_error) => {
                    self.is_generation_failed = true;
                    self.generation_error = Some(generation_error);
                    if self.pending_output_bytes.is_empty() {
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

    fn produce_next_byte(&mut self) -> Result<bool, SixelEncodeError> {
        loop {
            if self.header_byte_offset < self.header_bytes.len() {
                let output_byte = self.header_bytes[self.header_byte_offset];
                self.append_output_byte(output_byte)?;
                self.header_byte_offset += 1;
                return Ok(true);
            }

            if let Some(band) = self.current_band.as_mut() {
                if let Some(output_byte) = band.take_next_byte() {
                    self.append_output_byte(output_byte)?;
                    return Ok(true);
                }
                self.current_band = None;
                if self.next_band_start_row < self.image_height_pixels {
                    self.append_output_byte(b'-')?;
                    return Ok(true);
                }
                continue;
            }

            if self.next_band_start_row < self.image_height_pixels {
                let band_start_row = self.next_band_start_row;
                let band_height = (self.image_height_pixels - band_start_row).min(6);
                self.next_band_start_row += band_height;
                let built_band = build_sixel_band(
                    &self.decoded_image,
                    self.image_width_pixels,
                    band_start_row,
                    band_height,
                    self.background,
                    &self.palette,
                )?;
                if built_band.has_pixel_data() {
                    self.has_emitted_sixel = true;
                } else if !self.has_emitted_sixel {
                    self.has_emitted_sixel = true;
                    self.current_band = Some(built_band);
                    self.append_output_byte(b'?')?;
                    return Ok(true);
                }
                self.current_band = Some(built_band);
                continue;
            }

            if self.terminator_byte_offset < 2 {
                let output_byte = [0x1b, b'\\'][self.terminator_byte_offset];
                self.append_output_byte(output_byte)?;
                self.terminator_byte_offset += 1;
                if self.terminator_byte_offset == 2 {
                    self.is_finished = true;
                }
                return Ok(true);
            }
            self.is_finished = true;
            return Ok(false);
        }
    }

    fn append_output_byte(&mut self, output_byte: u8) -> Result<(), SixelEncodeError> {
        if self.generated_byte_count >= MAX_SIXEL_OUTPUT_BYTE_COUNT {
            return Err(SixelEncodeError::OutputTooLarge {
                maximum_byte_count: MAX_SIXEL_OUTPUT_BYTE_COUNT,
            });
        }
        if self.pending_output_bytes.len() >= MAX_SIXEL_CHUNK_BYTE_COUNT {
            return Err(SixelEncodeError::OutputTooLarge {
                maximum_byte_count: MAX_SIXEL_CHUNK_BYTE_COUNT,
            });
        }
        self.pending_output_bytes.push(output_byte);
        self.generated_byte_count += 1;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn inject_generation_failure_for_test(&mut self) {
        self.is_generation_failed = true;
        self.generation_error = Some(SixelEncodeError::PaletteMapping);
    }
}

#[derive(Debug, Clone)]
struct Palette {
    colors: Vec<[u8; 3]>,
    palette_mapping: PaletteMapping,
}

#[derive(Debug, Clone)]
enum PaletteMapping {
    Exact(BTreeMap<[u8; 3], u16>),
    Quantized(Vec<u16>),
}

impl Palette {
    fn find_palette_color_index(&self, color: [u8; 3]) -> Result<usize, SixelEncodeError> {
        match &self.palette_mapping {
            PaletteMapping::Exact(palette_index_by_color) => palette_index_by_color
                .get(&color)
                .copied()
                .map(usize::from)
                .ok_or(SixelEncodeError::PaletteMapping),
            PaletteMapping::Quantized(palette_index_by_histogram_bin) => {
                palette_index_by_histogram_bin
                    .get(compute_histogram_bin_index(color))
                    .copied()
                    .filter(|palette_index| usize::from(*palette_index) < self.colors.len())
                    .map(usize::from)
                    .ok_or(SixelEncodeError::PaletteMapping)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct HistogramBin {
    sample_count: u64,
    channel_sums: [u64; 3],
}

impl HistogramBin {
    fn add_color_sample(&mut self, sampled_color: [u8; 3]) {
        self.sample_count = self.sample_count.saturating_add(1);
        for (channel_sum, color_channel) in self.channel_sums.iter_mut().zip(sampled_color) {
            *channel_sum = channel_sum.saturating_add(u64::from(color_channel));
        }
    }

    fn compute_average_color(self) -> [u8; 3] {
        let sample_count = self.sample_count.max(1);
        [
            compute_rounded_average(self.channel_sums[0], sample_count),
            compute_rounded_average(self.channel_sums[1], sample_count),
            compute_rounded_average(self.channel_sums[2], sample_count),
        ]
    }
}

#[derive(Debug, Clone, Copy)]
struct HistogramColor {
    histogram_bin_index: usize,
    average_color: [u8; 3],
    sample_weight: u64,
}

#[derive(Debug)]
struct ColorBox {
    sample_indices: Vec<usize>,
    minimum_color: [u8; 3],
    maximum_color: [u8; 3],
    sample_weight: u64,
}

#[derive(Debug, Clone, Copy)]
struct BandEntry {
    pixel_column_index: usize,
    pixel_bit_mask: u8,
}

#[derive(Debug)]
struct BandState {
    band_entries_by_color: Vec<Vec<BandEntry>>,
    palette_color_index: usize,
    band_entry_index: usize,
    pixel_column_index: usize,
    stage: BandStage,
    has_emitted_color: bool,
    output_token_bytes: [u8; 32],
    output_token_byte_count: usize,
    output_token_byte_offset: usize,
    has_pixel_data: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BandStage {
    ColorSeparator,
    ColorNumber,
    Pixels,
}

impl BandState {
    fn from_band_entries(band_entries_by_color: Vec<Vec<BandEntry>>) -> Self {
        let has_pixel_data = band_entries_by_color
            .iter()
            .any(|color_entries| !color_entries.is_empty());
        BandState {
            band_entries_by_color,
            palette_color_index: 0,
            band_entry_index: 0,
            pixel_column_index: 0,
            stage: BandStage::ColorSeparator,
            has_emitted_color: false,
            output_token_bytes: [0; 32],
            output_token_byte_count: 0,
            output_token_byte_offset: 0,
            has_pixel_data,
        }
    }

    fn has_pixel_data(&self) -> bool {
        self.has_pixel_data
    }

    fn take_next_byte(&mut self) -> Option<u8> {
        loop {
            if self.output_token_byte_offset < self.output_token_byte_count {
                let output_byte = self.output_token_bytes[self.output_token_byte_offset];
                self.output_token_byte_offset += 1;
                return Some(output_byte);
            }

            while self.palette_color_index < self.band_entries_by_color.len()
                && self.band_entries_by_color[self.palette_color_index].is_empty()
            {
                self.palette_color_index += 1;
                self.band_entry_index = 0;
                self.pixel_column_index = 0;
                self.stage = BandStage::ColorSeparator;
            }
            if self.palette_color_index == self.band_entries_by_color.len() {
                return None;
            }

            match self.stage {
                BandStage::ColorSeparator => {
                    self.stage = BandStage::ColorNumber;
                    if self.has_emitted_color {
                        self.set_output_token_byte(b'$');
                    }
                    continue;
                }
                BandStage::ColorNumber => {
                    self.stage = BandStage::Pixels;
                    self.has_emitted_color = true;
                    self.set_color_token(self.palette_color_index);
                    continue;
                }
                BandStage::Pixels => {}
            }

            let (pixel_bit_mask, repeat_count, next_band_entry_index, next_pixel_column) = {
                let color_entries = &self.band_entries_by_color[self.palette_color_index];
                if self.band_entry_index >= color_entries.len() {
                    self.palette_color_index += 1;
                    self.band_entry_index = 0;
                    self.pixel_column_index = 0;
                    self.stage = BandStage::ColorSeparator;
                    continue;
                }
                let current_band_entry = color_entries[self.band_entry_index];
                if self.pixel_column_index < current_band_entry.pixel_column_index {
                    (
                        0,
                        current_band_entry.pixel_column_index - self.pixel_column_index,
                        self.band_entry_index,
                        current_band_entry.pixel_column_index,
                    )
                } else {
                    let mut last_pixel_column_index = current_band_entry.pixel_column_index;
                    let mut next_band_entry_index = self.band_entry_index + 1;
                    while next_band_entry_index < color_entries.len()
                        && color_entries[next_band_entry_index].pixel_column_index
                            == last_pixel_column_index + 1
                        && color_entries[next_band_entry_index].pixel_bit_mask
                            == current_band_entry.pixel_bit_mask
                    {
                        last_pixel_column_index += 1;
                        next_band_entry_index += 1;
                    }
                    (
                        current_band_entry.pixel_bit_mask,
                        last_pixel_column_index - current_band_entry.pixel_column_index + 1,
                        next_band_entry_index,
                        last_pixel_column_index + 1,
                    )
                }
            };
            self.band_entry_index = next_band_entry_index;
            self.pixel_column_index = next_pixel_column;
            self.set_pixel_token(repeat_count, pixel_bit_mask);
        }
    }

    fn set_output_token_byte(&mut self, output_byte: u8) {
        self.output_token_bytes[0] = output_byte;
        self.output_token_byte_count = 1;
        self.output_token_byte_offset = 0;
    }

    fn set_color_token(&mut self, color_index: usize) {
        self.output_token_bytes[0] = b'#';
        let digit_count = write_decimal_number(&mut self.output_token_bytes[1..], color_index);
        self.output_token_byte_count = digit_count + 1;
        self.output_token_byte_offset = 0;
    }

    fn set_pixel_token(&mut self, repeat_count: usize, pixel_bit_mask: u8) {
        self.output_token_byte_offset = 0;
        if repeat_count > 1 {
            self.output_token_bytes[0] = b'!';
            let digit_count = write_decimal_number(&mut self.output_token_bytes[1..], repeat_count);
            self.output_token_bytes[digit_count + 1] = b'?'.saturating_add(pixel_bit_mask);
            self.output_token_byte_count = digit_count + 2;
        } else {
            self.output_token_bytes[0] = b'?'.saturating_add(pixel_bit_mask);
            self.output_token_byte_count = 1;
        }
    }
}

fn validate_sixel_encode_options(
    encode_options: SixelEncodeOptions,
) -> Result<(), SixelEncodeError> {
    if !(MIN_PALETTE_COLOR_COUNT..=MAX_PALETTE_COLOR_COUNT)
        .contains(&encode_options.maximum_palette_color_count)
    {
        return Err(SixelEncodeError::InvalidPaletteSize {
            requested_palette_color_count: encode_options.maximum_palette_color_count,
        });
    }
    Ok(())
}

fn validate_decoded_image(
    decoded_image: &DecodedImage,
) -> Result<(usize, usize), SixelEncodeError> {
    let image_width_pixels = usize::try_from(decoded_image.pixel_width).map_err(|_| {
        SixelEncodeError::InvalidDimensions {
            pixel_width: decoded_image.pixel_width,
            pixel_height: decoded_image.pixel_height,
        }
    })?;
    let image_height_pixels = usize::try_from(decoded_image.pixel_height).map_err(|_| {
        SixelEncodeError::InvalidDimensions {
            pixel_width: decoded_image.pixel_width,
            pixel_height: decoded_image.pixel_height,
        }
    })?;
    validate_image_dimensions(
        GraphicsProtocol::Sixel,
        image_width_pixels,
        image_height_pixels,
    )
    .map_err(|_| SixelEncodeError::InvalidDimensions {
        pixel_width: decoded_image.pixel_width,
        pixel_height: decoded_image.pixel_height,
    })?;
    let expected_rgba_byte_count = compute_rgba_byte_count(
        GraphicsProtocol::Sixel,
        image_width_pixels,
        image_height_pixels,
    )
    .map_err(|_| SixelEncodeError::InvalidDimensions {
        pixel_width: decoded_image.pixel_width,
        pixel_height: decoded_image.pixel_height,
    })?;
    if decoded_image.rgba_bytes.len() != expected_rgba_byte_count {
        return Err(SixelEncodeError::RgbaLengthMismatch {
            expected_rgba_byte_count,
            actual_rgba_byte_count: decoded_image.rgba_bytes.len(),
        });
    }
    Ok((image_width_pixels, image_height_pixels))
}

fn resolve_chunk_byte_count(maximum_byte_count: usize) -> Result<usize, SixelEncodeError> {
    if maximum_byte_count == 0 {
        return Err(SixelEncodeError::ZeroChunkSize);
    }
    Ok(maximum_byte_count.min(MAX_SIXEL_CHUNK_BYTE_COUNT))
}

fn build_sixel_header(
    image_width_pixels: usize,
    image_height_pixels: usize,
    palette: &Palette,
) -> Result<Vec<u8>, SixelEncodeError> {
    let estimated_byte_count = 64usize.saturating_add(palette.colors.len().saturating_mul(20));
    let mut header_bytes = Vec::new();
    header_bytes
        .try_reserve(estimated_byte_count.min(MAX_SIXEL_CHUNK_BYTE_COUNT))
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    append_header_bytes(&mut header_bytes, b"\x1bP7;1q")?;
    append_header_byte(&mut header_bytes, b'"')?;
    append_header_bytes(&mut header_bytes, b"1;1;")?;
    append_header_decimal_number(&mut header_bytes, image_width_pixels)?;
    append_header_byte(&mut header_bytes, b';')?;
    append_header_decimal_number(&mut header_bytes, image_height_pixels)?;
    for (palette_index, palette_color) in palette.colors.iter().enumerate() {
        append_header_byte(&mut header_bytes, b'#')?;
        append_header_decimal_number(&mut header_bytes, palette_index)?;
        append_header_bytes(&mut header_bytes, b";2;")?;
        append_header_decimal_number(&mut header_bytes, usize::from(palette_color[0]))?;
        append_header_byte(&mut header_bytes, b';')?;
        append_header_decimal_number(&mut header_bytes, usize::from(palette_color[1]))?;
        append_header_byte(&mut header_bytes, b';')?;
        append_header_decimal_number(&mut header_bytes, usize::from(palette_color[2]))?;
    }
    Ok(header_bytes)
}

fn append_header_byte(header_bytes: &mut Vec<u8>, header_byte: u8) -> Result<(), SixelEncodeError> {
    append_header_bytes(header_bytes, &[header_byte])
}

fn append_header_decimal_number(
    header_bytes: &mut Vec<u8>,
    decimal_number: usize,
) -> Result<(), SixelEncodeError> {
    let mut decimal_digits = [0u8; 20];
    let digit_count = write_decimal_number(&mut decimal_digits, decimal_number);
    append_header_bytes(header_bytes, &decimal_digits[..digit_count])
}

fn append_header_bytes(
    header_bytes: &mut Vec<u8>,
    bytes_to_append: &[u8],
) -> Result<(), SixelEncodeError> {
    let new_header_byte_count = header_bytes
        .len()
        .checked_add(bytes_to_append.len())
        .ok_or(SixelEncodeError::OutputTooLarge {
            maximum_byte_count: MAX_SIXEL_OUTPUT_BYTE_COUNT,
        })?;
    if new_header_byte_count > MAX_SIXEL_CHUNK_BYTE_COUNT {
        return Err(SixelEncodeError::OutputTooLarge {
            maximum_byte_count: MAX_SIXEL_OUTPUT_BYTE_COUNT,
        });
    }
    if header_bytes.capacity().saturating_sub(header_bytes.len()) < bytes_to_append.len() {
        header_bytes
            .try_reserve_exact(bytes_to_append.len())
            .map_err(|_| SixelEncodeError::AllocationFailed)?;
    }
    header_bytes.extend_from_slice(bytes_to_append);
    Ok(())
}

fn write_decimal_number(output_bytes: &mut [u8], decimal_number: usize) -> usize {
    let mut decimal_digits = [0u8; 20];
    let mut digit_start_index = decimal_digits.len();
    let mut remaining_decimal_number = decimal_number;
    if remaining_decimal_number == 0 {
        digit_start_index -= 1;
        decimal_digits[digit_start_index] = b'0';
    } else {
        while remaining_decimal_number > 0 {
            digit_start_index -= 1;
            decimal_digits[digit_start_index] = b'0' + (remaining_decimal_number % 10) as u8;
            remaining_decimal_number /= 10;
        }
    }
    let digit_count = decimal_digits.len() - digit_start_index;
    output_bytes[..digit_count].copy_from_slice(&decimal_digits[digit_start_index..]);
    digit_count
}

fn prepare_sixel_palette(
    rgba_bytes: &[u8],
    background: [u8; 3],
    maximum_palette_color_count: usize,
) -> Result<Palette, SixelEncodeError> {
    let mut seen_colors = BTreeSet::new();
    let mut palette_colors = Vec::new();
    palette_colors
        .try_reserve_exact(maximum_palette_color_count)
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    for pixel_rgba_bytes in rgba_bytes.chunks_exact(4) {
        let Some(pixel_color) = blend_pixel_to_sixel_percentage(pixel_rgba_bytes, background)
        else {
            continue;
        };
        if seen_colors.insert(pixel_color) {
            palette_colors.push(pixel_color);
            if palette_colors.len() > maximum_palette_color_count {
                return prepare_quantized_sixel_palette(
                    rgba_bytes,
                    background,
                    maximum_palette_color_count,
                );
            }
        }
    }
    let mut palette_mapping = BTreeMap::new();
    for (palette_index, palette_color) in palette_colors.iter().copied().enumerate() {
        palette_mapping.insert(
            palette_color,
            u16::try_from(palette_index).map_err(|_| SixelEncodeError::PaletteMapping)?,
        );
    }
    Ok(Palette {
        colors: palette_colors,
        palette_mapping: PaletteMapping::Exact(palette_mapping),
    })
}

fn prepare_quantized_sixel_palette(
    rgba_bytes: &[u8],
    background: [u8; 3],
    maximum_palette_color_count: usize,
) -> Result<Palette, SixelEncodeError> {
    let mut histogram_bins = Vec::new();
    histogram_bins
        .try_reserve_exact(HISTOGRAM_BUCKET_COUNT)
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    histogram_bins.resize(HISTOGRAM_BUCKET_COUNT, HistogramBin::default());
    for pixel_rgba_bytes in rgba_bytes.chunks_exact(4) {
        let Some(pixel_color) = blend_pixel_to_sixel_percentage(pixel_rgba_bytes, background)
        else {
            continue;
        };
        histogram_bins[compute_histogram_bin_index(pixel_color)].add_color_sample(pixel_color);
    }

    let mut histogram_samples = Vec::new();
    histogram_samples
        .try_reserve_exact(HISTOGRAM_BUCKET_COUNT)
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    for (histogram_bin_index, histogram_bin) in histogram_bins.into_iter().enumerate() {
        if histogram_bin.sample_count == 0 {
            continue;
        }
        histogram_samples.push(HistogramColor {
            histogram_bin_index,
            average_color: histogram_bin.compute_average_color(),
            sample_weight: histogram_bin.sample_count,
        });
    }
    if histogram_samples.is_empty() {
        return Ok(Palette {
            colors: Vec::new(),
            palette_mapping: PaletteMapping::Exact(BTreeMap::new()),
        });
    }

    let mut initial_sample_indices = Vec::new();
    initial_sample_indices
        .try_reserve_exact(histogram_samples.len())
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    initial_sample_indices.extend(0..histogram_samples.len());
    let mut color_boxes = Vec::new();
    color_boxes
        .try_reserve_exact(maximum_palette_color_count)
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    color_boxes.push(build_color_box(initial_sample_indices, &histogram_samples));
    while color_boxes.len() < maximum_palette_color_count {
        let Some(color_box_index) = color_boxes
            .iter()
            .enumerate()
            .filter(|(_, color_box)| {
                color_box.sample_indices.len() > 1 && compute_color_box_range(color_box) > 0
            })
            .max_by_key(|(color_box_index, color_box)| {
                (
                    compute_color_box_range(color_box),
                    color_box.sample_weight,
                    std::cmp::Reverse(*color_box_index),
                )
            })
            .map(|(color_box_index, _)| color_box_index)
        else {
            break;
        };
        let split_channel_index = choose_split_channel(&color_boxes[color_box_index]);
        let color_box = &mut color_boxes[color_box_index];
        color_box.sample_indices.sort_by_key(|sample_index| {
            let histogram_sample = histogram_samples[*sample_index];
            (
                histogram_sample.average_color[split_channel_index],
                histogram_sample.average_color[(split_channel_index + 1) % 3],
                histogram_sample.average_color[(split_channel_index + 2) % 3],
                histogram_sample.histogram_bin_index,
            )
        });
        let target_sample_weight = color_box.sample_weight.saturating_add(1) / 2;
        let mut cumulative_sample_weight = 0u64;
        let mut split_member_index = color_box.sample_indices.len() / 2;
        for (member_index, sample_index) in color_box.sample_indices.iter().enumerate() {
            cumulative_sample_weight = cumulative_sample_weight
                .saturating_add(histogram_samples[*sample_index].sample_weight);
            if cumulative_sample_weight >= target_sample_weight {
                split_member_index = member_index + 1;
                break;
            }
        }
        split_member_index = split_member_index.clamp(1, color_box.sample_indices.len() - 1);

        let sample_indices = std::mem::take(&mut color_box.sample_indices);
        let right_sample_count = sample_indices.len() - split_member_index;
        let mut left_sample_indices = Vec::new();
        left_sample_indices
            .try_reserve_exact(split_member_index)
            .map_err(|_| SixelEncodeError::AllocationFailed)?;
        let mut right_sample_indices = Vec::new();
        right_sample_indices
            .try_reserve_exact(right_sample_count)
            .map_err(|_| SixelEncodeError::AllocationFailed)?;
        for (sample_position, sample_index) in sample_indices.into_iter().enumerate() {
            if sample_position < split_member_index {
                left_sample_indices.push(sample_index);
            } else {
                right_sample_indices.push(sample_index);
            }
        }
        let left_color_box = build_color_box(left_sample_indices, &histogram_samples);
        let right_color_box = build_color_box(right_sample_indices, &histogram_samples);
        color_boxes[color_box_index] = left_color_box;
        color_boxes.push(right_color_box);
    }

    let mut palette_colors = Vec::new();
    palette_colors
        .try_reserve_exact(color_boxes.len())
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    let mut histogram_to_palette_mapping = Vec::new();
    histogram_to_palette_mapping
        .try_reserve_exact(HISTOGRAM_BUCKET_COUNT)
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    histogram_to_palette_mapping.resize(HISTOGRAM_BUCKET_COUNT, u16::MAX);
    for (palette_index, color_box) in color_boxes.into_iter().enumerate() {
        let mut channel_sums = [0u64; 3];
        let mut sample_weight = 0u64;
        for sample_index in color_box.sample_indices {
            let histogram_sample = histogram_samples[sample_index];
            sample_weight = sample_weight.saturating_add(histogram_sample.sample_weight);
            for (channel_sum, color_channel) in
                channel_sums.iter_mut().zip(histogram_sample.average_color)
            {
                *channel_sum = channel_sum.saturating_add(
                    u64::from(color_channel).saturating_mul(histogram_sample.sample_weight),
                );
            }
            histogram_to_palette_mapping[histogram_sample.histogram_bin_index] =
                u16::try_from(palette_index).map_err(|_| SixelEncodeError::PaletteMapping)?;
        }
        let sample_weight = sample_weight.max(1);
        palette_colors.push([
            compute_rounded_average(channel_sums[0], sample_weight),
            compute_rounded_average(channel_sums[1], sample_weight),
            compute_rounded_average(channel_sums[2], sample_weight),
        ]);
    }
    Ok(Palette {
        colors: palette_colors,
        palette_mapping: PaletteMapping::Quantized(histogram_to_palette_mapping),
    })
}

fn build_color_box(sample_indices: Vec<usize>, histogram_samples: &[HistogramColor]) -> ColorBox {
    let mut minimum_color = [u8::MAX; 3];
    let mut maximum_color = [0; 3];
    let mut sample_weight = 0u64;
    for sample_index in &sample_indices {
        let histogram_sample = histogram_samples[*sample_index];
        sample_weight = sample_weight.saturating_add(histogram_sample.sample_weight);
        for color_channel_index in 0..3 {
            minimum_color[color_channel_index] = minimum_color[color_channel_index]
                .min(histogram_sample.average_color[color_channel_index]);
            maximum_color[color_channel_index] = maximum_color[color_channel_index]
                .max(histogram_sample.average_color[color_channel_index]);
        }
    }
    ColorBox {
        sample_indices,
        minimum_color,
        maximum_color,
        sample_weight,
    }
}

fn compute_color_box_range(color_box: &ColorBox) -> u8 {
    color_box
        .maximum_color
        .iter()
        .zip(color_box.minimum_color)
        .map(|(maximum_color, minimum_color)| maximum_color.saturating_sub(minimum_color))
        .max()
        .unwrap_or(0)
}

fn choose_split_channel(color_box: &ColorBox) -> usize {
    let ranges = [
        color_box.maximum_color[0].saturating_sub(color_box.minimum_color[0]),
        color_box.maximum_color[1].saturating_sub(color_box.minimum_color[1]),
        color_box.maximum_color[2].saturating_sub(color_box.minimum_color[2]),
    ];
    ranges
        .iter()
        .enumerate()
        .max_by_key(|(color_channel_index, color_range)| {
            (**color_range, std::cmp::Reverse(*color_channel_index))
        })
        .map(|(color_channel_index, _)| color_channel_index)
        .unwrap_or(0)
}

fn build_sixel_band(
    decoded_image: &DecodedImage,
    image_width_pixels: usize,
    band_start_row: usize,
    band_height: usize,
    background: [u8; 3],
    palette: &Palette,
) -> Result<BandState, SixelEncodeError> {
    let mut band_entries_by_color = Vec::new();
    band_entries_by_color
        .try_reserve_exact(palette.colors.len())
        .map_err(|_| SixelEncodeError::AllocationFailed)?;
    band_entries_by_color.resize_with(palette.colors.len(), Vec::new);

    for band_row_offset in 0..band_height {
        let image_row = band_start_row + band_row_offset;
        for image_column in 0..image_width_pixels {
            let rgba_byte_offset = (image_row * image_width_pixels + image_column) * 4;
            let Some(pixel_color) = blend_pixel_to_sixel_percentage(
                &decoded_image.rgba_bytes[rgba_byte_offset..rgba_byte_offset + 4],
                background,
            ) else {
                continue;
            };
            let palette_color_index = palette.find_palette_color_index(pixel_color)?;
            let color_entries = &mut band_entries_by_color[palette_color_index];
            color_entries
                .try_reserve(1)
                .map_err(|_| SixelEncodeError::AllocationFailed)?;
            color_entries.push(BandEntry {
                pixel_column_index: image_column,
                pixel_bit_mask: 1 << band_row_offset,
            });
        }
    }

    for color_entries in &mut band_entries_by_color {
        color_entries.sort_unstable_by_key(|band_entry| band_entry.pixel_column_index);
        let mut unique_entry_count = 0;
        for entry_read_index in 0..color_entries.len() {
            let band_entry = color_entries[entry_read_index];
            if unique_entry_count > 0
                && color_entries[unique_entry_count - 1].pixel_column_index
                    == band_entry.pixel_column_index
            {
                color_entries[unique_entry_count - 1].pixel_bit_mask |= band_entry.pixel_bit_mask;
            } else {
                color_entries[unique_entry_count] = band_entry;
                unique_entry_count += 1;
            }
        }
        color_entries.truncate(unique_entry_count);
    }
    Ok(BandState::from_band_entries(band_entries_by_color))
}

fn blend_pixel_to_sixel_percentage(
    pixel_rgba_bytes: &[u8],
    background: [u8; 3],
) -> Option<[u8; 3]> {
    let alpha_byte = pixel_rgba_bytes[3];
    if alpha_byte == 0 {
        return None;
    }
    let inverse_alpha = 255u16 - u16::from(alpha_byte);
    Some([
        convert_byte_to_percentage(blend_channel(
            pixel_rgba_bytes[0],
            background[0],
            alpha_byte,
            inverse_alpha,
        )),
        convert_byte_to_percentage(blend_channel(
            pixel_rgba_bytes[1],
            background[1],
            alpha_byte,
            inverse_alpha,
        )),
        convert_byte_to_percentage(blend_channel(
            pixel_rgba_bytes[2],
            background[2],
            alpha_byte,
            inverse_alpha,
        )),
    ])
}

fn blend_channel(source_byte: u8, background_byte: u8, alpha: u8, inverse_alpha: u16) -> u8 {
    let numerator = u32::from(source_byte) * u32::from(alpha)
        + u32::from(background_byte) * u32::from(inverse_alpha)
        + 127;
    (numerator / 255) as u8
}

fn convert_byte_to_percentage(color_channel_byte: u8) -> u8 {
    ((u16::from(color_channel_byte) * 100 + 127) / 255) as u8
}

fn compute_rounded_average(channel_sum: u64, sample_count: u64) -> u8 {
    ((channel_sum.saturating_add(sample_count / 2)) / sample_count.max(1)).min(100) as u8
}

fn compute_histogram_bin_index(pixel_color: [u8; 3]) -> usize {
    let red_bin_index = usize::from(pixel_color[0]) * HISTOGRAM_BUCKET_COUNT_PER_CHANNEL / 101;
    let green_bin_index = usize::from(pixel_color[1]) * HISTOGRAM_BUCKET_COUNT_PER_CHANNEL / 101;
    let blue_bin_index = usize::from(pixel_color[2]) * HISTOGRAM_BUCKET_COUNT_PER_CHANNEL / 101;
    (red_bin_index * HISTOGRAM_BUCKET_COUNT_PER_CHANNEL + green_bin_index)
        * HISTOGRAM_BUCKET_COUNT_PER_CHANNEL
        + blue_bin_index
}
